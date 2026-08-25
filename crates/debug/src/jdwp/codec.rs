//! JDWP wire codec: big-endian packet payload reader/writer, ID-size-aware
//! object/type/method/frame ID encoding, values, locations, and Composite
//! event parsing.
//!
//! Everything decoded here came from the debuggee JVM and is **untrusted**:
//! reads are bounds-checked (never panic), structural strings beyond
//! [`MAX_STRING_LEN`] are rejected, and value strings are truncated. A decode
//! failure surfaces as [`DecodeError`] and tears the debug session down.

use super::commands::{event_kind, tag};

/// Cap on an accepted JDWP packet's declared length — a hostile debuggee
/// cannot make the adapter buffer unbounded memory.
pub const MAX_PACKET_LEN: u32 = 16 * 1024 * 1024;

/// Cap on decoded UTF-8 strings (structural strings — signatures, names —
/// are rejected beyond this; value strings are truncated instead).
pub const MAX_STRING_LEN: usize = 64 * 1024;

/// A malformed/oversized reply or event from the debuggee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed JDWP data: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// Sizes, in bytes, of the variably-sized JDWP identifiers, as reported by
/// `VirtualMachine.IDSizes` — every ID codec after the first command uses
/// these. HotSpot reports 8 for all of them, but the protocol allows 1–8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSizes {
    pub field: usize,
    pub method: usize,
    pub object: usize,
    pub reference_type: usize,
    pub frame: usize,
}

impl IdSizes {
    fn valid(self) -> bool {
        [
            self.field,
            self.method,
            self.object,
            self.reference_type,
            self.frame,
        ]
        .iter()
        .all(|&s| (1..=8).contains(&s))
    }
}

/// A code location: class + method + bytecode index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub type_tag: u8,
    pub class_id: u64,
    pub method_id: u64,
    pub index: u64,
}

/// A JDWP value (tagged or untagged-with-known-tag).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Void,
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// Any object-family value (object, string, array, thread, …); `id == 0`
    /// is `null`. `tag` is the runtime tag the VM reported.
    Object {
        tag: u8,
        id: u64,
    },
}

/// Bounds-checked big-endian reader over a packet payload.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError(format!(
                "truncated payload: wanted {n} bytes, {} left",
                self.remaining()
            )));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// A variably-sized (1–8 byte) big-endian ID.
    pub fn id(&mut self, size: usize) -> Result<u64, DecodeError> {
        let b = self.take(size)?;
        let mut v = 0u64;
        for &byte in b {
            v = (v << 8) | u64::from(byte);
        }
        Ok(v)
    }

    /// A structural string (signature/name): length-prefixed UTF-8, rejected
    /// beyond [`MAX_STRING_LEN`].
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        if len > MAX_STRING_LEN {
            return Err(DecodeError(format!("structural string too long: {len}")));
        }
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// A value string (e.g. `StringReference.Value`): truncated (not
    /// rejected) at [`MAX_STRING_LEN`]; the rest of the bytes are skipped so
    /// the reader stays positioned correctly.
    pub fn value_string(&mut self) -> Result<String, DecodeError> {
        let len = self.u32()? as usize;
        let keep = len.min(MAX_STRING_LEN);
        let bytes = self.take(keep)?;
        let s = String::from_utf8_lossy(bytes).into_owned();
        if len > keep {
            self.take(len - keep)?;
        }
        Ok(s)
    }

    pub fn location(&mut self, sizes: IdSizes) -> Result<Location, DecodeError> {
        Ok(Location {
            type_tag: self.u8()?,
            class_id: self.id(sizes.reference_type)?,
            method_id: self.id(sizes.method)?,
            index: self.u64()?,
        })
    }

    /// A tagged value: tag byte, then the tag-determined payload.
    pub fn tagged_value(&mut self, sizes: IdSizes) -> Result<Value, DecodeError> {
        let t = self.u8()?;
        self.untagged_value(t, sizes)
    }

    /// A value whose tag is already known (array regions send primitive
    /// elements untagged).
    pub fn untagged_value(&mut self, t: u8, sizes: IdSizes) -> Result<Value, DecodeError> {
        Ok(match t {
            tag::VOID => Value::Void,
            tag::BOOLEAN => Value::Boolean(self.u8()? != 0),
            tag::BYTE => Value::Byte(self.u8()? as i8),
            tag::CHAR => Value::Char(self.u16()?),
            tag::SHORT => Value::Short(self.u16()? as i16),
            tag::INT => Value::Int(self.u32()? as i32),
            tag::LONG => Value::Long(self.u64()? as i64),
            tag::FLOAT => Value::Float(f32::from_bits(self.u32()?)),
            tag::DOUBLE => Value::Double(f64::from_bits(self.u64()?)),
            tag::OBJECT
            | tag::STRING
            | tag::ARRAY
            | tag::THREAD
            | tag::THREAD_GROUP
            | tag::CLASS_LOADER
            | tag::CLASS_OBJECT => Value::Object {
                tag: t,
                id: self.id(sizes.object)?,
            },
            other => return Err(DecodeError(format!("unknown value tag {other}"))),
        })
    }
}

/// Big-endian payload builder.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn id(&mut self, size: usize, v: u64) -> &mut Self {
        // Big-endian, `size` low-order bytes.
        for i in (0..size).rev() {
            self.buf.push((v >> (8 * i)) as u8);
        }
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }

    pub fn location(&mut self, sizes: IdSizes, loc: Location) -> &mut Self {
        self.u8(loc.type_tag);
        self.id(sizes.reference_type, loc.class_id);
        self.id(sizes.method, loc.method_id);
        self.u64(loc.index)
    }
}

/// Parse a `VirtualMachine.IDSizes` reply.
pub fn parse_id_sizes(payload: &[u8]) -> Result<IdSizes, DecodeError> {
    let mut r = Reader::new(payload);
    let sizes = IdSizes {
        field: r.u32()? as usize,
        method: r.u32()? as usize,
        object: r.u32()? as usize,
        reference_type: r.u32()? as usize,
        frame: r.u32()? as usize,
    };
    if !sizes.valid() {
        return Err(DecodeError(format!("implausible ID sizes: {sizes:?}")));
    }
    Ok(sizes)
}

/// One event out of a Composite event packet.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    VmStart {
        request_id: u32,
        thread: u64,
    },
    SingleStep {
        request_id: u32,
        thread: u64,
        location: Location,
    },
    Breakpoint {
        request_id: u32,
        thread: u64,
        location: Location,
    },
    Exception {
        request_id: u32,
        thread: u64,
        location: Location,
        exception: u64,
        catch_location: Option<Location>,
    },
    ClassPrepare {
        request_id: u32,
        thread: u64,
        type_tag: u8,
        type_id: u64,
        signature: String,
        status: u32,
    },
    VmDeath {
        request_id: u32,
    },
}

/// A parsed Composite event packet (set 64, command 100).
#[derive(Debug, Clone, PartialEq)]
pub struct EventSet {
    pub suspend_policy: u8,
    pub events: Vec<Event>,
}

/// Cap on events in one composite — defensive, a real VM sends few.
const MAX_EVENTS_PER_COMPOSITE: u32 = 1024;

/// Parse a Composite event payload. An unknown event kind is a decode error
/// (its payload size is unknowable, so nothing after it could be trusted);
/// only kinds this adapter requests — plus the automatic VM_START/VM_DEATH —
/// ever appear.
pub fn parse_composite(payload: &[u8], sizes: IdSizes) -> Result<EventSet, DecodeError> {
    let mut r = Reader::new(payload);
    let suspend_policy = r.u8()?;
    let count = r.u32()?;
    if count > MAX_EVENTS_PER_COMPOSITE {
        return Err(DecodeError(format!("composite event count {count}")));
    }
    let mut events = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = r.u8()?;
        let request_id = r.u32()?;
        let event = match kind {
            event_kind::VM_START => Event::VmStart {
                request_id,
                thread: r.id(sizes.object)?,
            },
            event_kind::SINGLE_STEP => Event::SingleStep {
                request_id,
                thread: r.id(sizes.object)?,
                location: r.location(sizes)?,
            },
            event_kind::BREAKPOINT => Event::Breakpoint {
                request_id,
                thread: r.id(sizes.object)?,
                location: r.location(sizes)?,
            },
            event_kind::EXCEPTION => {
                let thread = r.id(sizes.object)?;
                let location = r.location(sizes)?;
                // The exception object is a *tagged* objectID.
                let _tag = r.u8()?;
                let exception = r.id(sizes.object)?;
                let catch_location = r.location(sizes)?;
                Event::Exception {
                    request_id,
                    thread,
                    location,
                    exception,
                    // "0 if uncaught" — a null catch location has class 0.
                    catch_location: (catch_location.class_id != 0).then_some(catch_location),
                }
            }
            event_kind::CLASS_PREPARE => Event::ClassPrepare {
                request_id,
                thread: r.id(sizes.object)?,
                type_tag: r.u8()?,
                type_id: r.id(sizes.reference_type)?,
                signature: r.string()?,
                status: r.u32()?,
            },
            event_kind::VM_DEATH => Event::VmDeath { request_id },
            other => return Err(DecodeError(format!("unknown event kind {other}"))),
        };
        events.push(event);
    }
    Ok(EventSet {
        suspend_policy,
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jdwp::commands::suspend_policy;

    const SIZES_8: IdSizes = IdSizes {
        field: 8,
        method: 8,
        object: 8,
        reference_type: 8,
        frame: 8,
    };

    #[test]
    fn writer_reader_roundtrip_with_odd_id_sizes() {
        let sizes = IdSizes {
            field: 4,
            method: 4,
            object: 5,
            reference_type: 3,
            frame: 2,
        };
        let loc = Location {
            type_tag: 1,
            class_id: 0x0A_BB_CC,
            method_id: 0xDEAD_BEEF,
            index: 42,
        };
        let mut w = Writer::new();
        w.u8(7)
            .u32(0x01020304)
            .u64(0x1122334455667788)
            .id(sizes.object, 0x1F_2233_4455)
            .string("Ldemo/Main;")
            .location(sizes, loc);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 0x01020304);
        assert_eq!(r.u64().unwrap(), 0x1122334455667788);
        assert_eq!(r.id(sizes.object).unwrap(), 0x1F_2233_4455);
        assert_eq!(r.string().unwrap(), "Ldemo/Main;");
        assert_eq!(r.location(sizes).unwrap(), loc);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn truncated_payload_is_an_error_not_a_panic() {
        let mut r = Reader::new(&[0, 0, 0]);
        assert!(r.u32().is_err());
        // A string whose declared length exceeds the buffer.
        let mut r = Reader::new(&[0, 0, 0, 10, b'a']);
        assert!(r.string().is_err());
    }

    #[test]
    fn oversized_structural_string_rejected_value_string_truncated() {
        // Declared length above the cap: structural read must reject
        // without attempting to consume the (absent) bytes.
        let huge = (MAX_STRING_LEN as u32 + 1).to_be_bytes();
        let mut r = Reader::new(&huge);
        assert!(r.string().is_err());

        // Value string: declared length above cap, bytes actually present —
        // truncated to the cap and the reader consumes everything.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_STRING_LEN as u32 + 3).to_be_bytes());
        buf.extend(std::iter::repeat_n(b'x', MAX_STRING_LEN + 3));
        let mut r = Reader::new(&buf);
        let s = r.value_string().unwrap();
        assert_eq!(s.len(), MAX_STRING_LEN);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn parse_id_sizes_accepts_hotspot_and_rejects_junk() {
        let mut w = Writer::new();
        w.u32(8).u32(8).u32(8).u32(8).u32(8);
        assert_eq!(parse_id_sizes(&w.into_bytes()).unwrap(), SIZES_8);

        let mut w = Writer::new();
        w.u32(8).u32(0).u32(8).u32(8).u32(8);
        assert!(parse_id_sizes(&w.into_bytes()).is_err());
        let mut w = Writer::new();
        w.u32(8).u32(9).u32(8).u32(8).u32(8);
        assert!(parse_id_sizes(&w.into_bytes()).is_err());
    }

    #[test]
    fn composite_event_parses_breakpoint_and_class_prepare() {
        let loc = Location {
            type_tag: 1,
            class_id: 100,
            method_id: 200,
            index: 7,
        };
        let mut w = Writer::new();
        w.u8(suspend_policy::ALL).u32(2);
        // BREAKPOINT event.
        w.u8(super::event_kind::BREAKPOINT)
            .u32(11)
            .id(SIZES_8.object, 55)
            .location(SIZES_8, loc);
        // CLASS_PREPARE event.
        w.u8(super::event_kind::CLASS_PREPARE)
            .u32(12)
            .id(SIZES_8.object, 55)
            .u8(1)
            .id(SIZES_8.reference_type, 100)
            .string("Ldemo/Main;")
            .u32(7);
        let set = parse_composite(&w.into_bytes(), SIZES_8).unwrap();
        assert_eq!(set.suspend_policy, suspend_policy::ALL);
        assert_eq!(
            set.events[0],
            Event::Breakpoint {
                request_id: 11,
                thread: 55,
                location: loc
            }
        );
        assert_eq!(
            set.events[1],
            Event::ClassPrepare {
                request_id: 12,
                thread: 55,
                type_tag: 1,
                type_id: 100,
                signature: "Ldemo/Main;".to_string(),
                status: 7
            }
        );
    }

    #[test]
    fn composite_event_uncaught_exception_has_no_catch_location() {
        let loc = Location {
            type_tag: 1,
            class_id: 100,
            method_id: 200,
            index: 7,
        };
        let null_loc = Location {
            type_tag: 0,
            class_id: 0,
            method_id: 0,
            index: 0,
        };
        let mut w = Writer::new();
        w.u8(suspend_policy::ALL).u32(1);
        w.u8(super::event_kind::EXCEPTION)
            .u32(9)
            .id(SIZES_8.object, 55)
            .location(SIZES_8, loc)
            .u8(crate::jdwp::commands::tag::OBJECT)
            .id(SIZES_8.object, 777)
            .location(SIZES_8, null_loc);
        let set = parse_composite(&w.into_bytes(), SIZES_8).unwrap();
        assert_eq!(
            set.events[0],
            Event::Exception {
                request_id: 9,
                thread: 55,
                location: loc,
                exception: 777,
                catch_location: None
            }
        );
    }

    #[test]
    fn unknown_event_kind_is_a_decode_error() {
        let mut w = Writer::new();
        w.u8(suspend_policy::ALL).u32(1).u8(250).u32(1);
        assert!(parse_composite(&w.into_bytes(), SIZES_8).is_err());
    }
}
