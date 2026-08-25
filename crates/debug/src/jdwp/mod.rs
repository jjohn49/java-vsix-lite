//! Async JDWP client over `tokio::net::TcpStream`.
//!
//! One reader task dispatches reply packets by id to `oneshot` senders in a
//! pending map and forwards Composite event payloads (raw bytes — parsed by
//! the session once `IDSizes` is known, since the automatic `VM_START` event
//! can arrive before the `IDSizes` reply) into an `mpsc` channel. All packet
//! lengths are capped ([`codec::MAX_PACKET_LEN`]) because the debuggee is
//! untrusted input.

pub mod codec;
pub mod commands;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use codec::{DecodeError, IdSizes, Location, Reader, Value, Writer};

/// Any failure talking JDWP: transport, malformed data, an error reply, or
/// a torn-down connection.
#[derive(Debug)]
pub enum JdwpError {
    Io(std::io::Error),
    Decode(DecodeError),
    /// A non-zero JDWP error code in a reply.
    Code(u16),
    /// The connection is gone (VM death, socket EOF, or reader teardown).
    Closed,
}

impl std::fmt::Display for JdwpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JdwpError::Io(e) => write!(f, "JDWP I/O error: {e}"),
            JdwpError::Decode(e) => write!(f, "{e}"),
            JdwpError::Code(c) => write!(f, "JDWP error code {c}"),
            JdwpError::Closed => write!(f, "JDWP connection closed"),
        }
    }
}

impl std::error::Error for JdwpError {}

impl From<std::io::Error> for JdwpError {
    fn from(e: std::io::Error) -> JdwpError {
        JdwpError::Io(e)
    }
}

impl From<DecodeError> for JdwpError {
    fn from(e: DecodeError) -> JdwpError {
        JdwpError::Decode(e)
    }
}

impl JdwpError {
    /// Whether this is the expected `ABSENT_INFORMATION` reply (class
    /// compiled without `-g`) — non-fatal for line/variable tables.
    pub fn is_absent_information(&self) -> bool {
        matches!(self, JdwpError::Code(commands::error::ABSENT_INFORMATION))
    }
}

const HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";
const FLAG_REPLY: u8 = 0x80;
const HEADER_LEN: u32 = 11;

type Pending = Arc<Mutex<Option<HashMap<u32, oneshot::Sender<Result<Vec<u8>, JdwpError>>>>>>;

/// Low-level connection handle: assigns ids, writes command packets, and
/// hands replies back through the pending map the reader task fills.
pub struct JdwpClient {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU32,
}

impl JdwpClient {
    /// Perform the 14-byte `JDWP-Handshake` exchange and start the reader
    /// task. Returns the client plus the channel of raw Composite event
    /// payloads; the channel closing means the connection died.
    pub async fn connect(
        mut stream: TcpStream,
    ) -> Result<(Arc<JdwpClient>, mpsc::Receiver<Vec<u8>>), JdwpError> {
        stream.write_all(HANDSHAKE).await?;
        let mut echo = [0u8; 14];
        stream.read_exact(&mut echo).await?;
        if &echo != HANDSHAKE {
            return Err(JdwpError::Decode(DecodeError(
                "bad JDWP handshake reply".to_string(),
            )));
        }
        let (read_half, write_half) = stream.into_split();
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (event_tx, event_rx) = mpsc::channel(64);
        let client = Arc::new(JdwpClient {
            writer: tokio::sync::Mutex::new(write_half),
            pending: pending.clone(),
            next_id: AtomicU32::new(1),
        });
        tokio::spawn(reader_task(read_half, pending, event_tx));
        Ok((client, event_rx))
    }

    /// Send one command and await its reply payload.
    pub async fn command(&self, set: u8, cmd: u8, payload: Vec<u8>) -> Result<Vec<u8>, JdwpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().expect("jdwp pending map");
            match guard.as_mut() {
                Some(map) => {
                    map.insert(id, tx);
                }
                None => return Err(JdwpError::Closed),
            }
        }
        let mut packet = Vec::with_capacity(11 + payload.len());
        packet.extend_from_slice(&(HEADER_LEN + payload.len() as u32).to_be_bytes());
        packet.extend_from_slice(&id.to_be_bytes());
        packet.push(0); // flags
        packet.push(set);
        packet.push(cmd);
        packet.extend_from_slice(&payload);
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(&packet).await {
                self.pending
                    .lock()
                    .expect("jdwp pending map")
                    .as_mut()
                    .and_then(|map| map.remove(&id));
                return Err(e.into());
            }
        }
        rx.await.unwrap_or(Err(JdwpError::Closed))
    }
}

/// Read packets forever: replies go to their pending waiter, Composite
/// events go to `event_tx` raw. On any error/EOF, fail every pending waiter
/// and drop the event channel so the session observes termination.
async fn reader_task(
    mut read_half: OwnedReadHalf,
    pending: Pending,
    event_tx: mpsc::Sender<Vec<u8>>,
) {
    loop {
        let mut header = [0u8; 11];
        if read_half.read_exact(&mut header).await.is_err() {
            break;
        }
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        if !(HEADER_LEN..=codec::MAX_PACKET_LEN).contains(&length) {
            tracing::warn!(length, "JDWP packet length out of bounds; closing");
            break;
        }
        let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let flags = header[8];
        let mut payload = vec![0u8; (length - HEADER_LEN) as usize];
        if read_half.read_exact(&mut payload).await.is_err() {
            break;
        }
        if flags & FLAG_REPLY != 0 {
            let error_code = u16::from_be_bytes([header[9], header[10]]);
            let waiter = pending
                .lock()
                .expect("jdwp pending map")
                .as_mut()
                .and_then(|map| map.remove(&id));
            if let Some(tx) = waiter {
                let result = if error_code == 0 {
                    Ok(payload)
                } else {
                    Err(JdwpError::Code(error_code))
                };
                let _ = tx.send(result);
            }
        } else if header[9] == commands::event::SET
            && header[10] == commands::event::COMPOSITE
            && event_tx.send(payload).await.is_err()
        {
            break; // session gone
        }
        // Any other VM→debugger command: ignored (none are expected).
    }
    // Teardown: fail all pending and refuse new commands.
    let map = pending.lock().expect("jdwp pending map").take();
    if let Some(map) = map {
        for (_, tx) in map {
            let _ = tx.send(Err(JdwpError::Closed));
        }
    }
}

/// A method as reported by `ReferenceType.Methods`.
#[derive(Debug, Clone)]
pub struct MethodEntry {
    pub id: u64,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

/// A field as reported by `ReferenceType.Fields`.
#[derive(Debug, Clone)]
pub struct FieldEntry {
    pub id: u64,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

/// A `Method.LineTable` reply.
#[derive(Debug, Clone)]
pub struct LineTable {
    pub start: i64,
    pub end: i64,
    /// `(code index, line number)` pairs.
    pub lines: Vec<(u64, u32)>,
}

/// One `Method.VariableTable` slot.
#[derive(Debug, Clone)]
pub struct VariableSlot {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub length: u32,
    pub slot: u32,
}

/// One frame from `ThreadReference.Frames`.
#[derive(Debug, Clone, Copy)]
pub struct FrameEntry {
    pub frame_id: u64,
    pub location: Location,
}

/// A class from `VirtualMachine.AllClasses`/`ClassesBySignature`.
#[derive(Debug, Clone)]
pub struct ClassEntry {
    pub type_tag: u8,
    pub type_id: u64,
    pub signature: String,
    pub status: u32,
}

/// An `EventRequest.Set` modifier.
#[derive(Debug, Clone)]
pub enum Modifier {
    Count(u32),
    ClassMatch(String),
    LocationOnly(Location),
    ExceptionOnly { caught: bool, uncaught: bool },
    Step { thread: u64, size: u32, depth: u32 },
}

/// Typed JDWP commands over a connected client, using the VM-reported ID
/// sizes. This is the only surface the DAP session talks to.
pub struct Vm {
    client: Arc<JdwpClient>,
    pub sizes: IdSizes,
}

impl Vm {
    /// Issue `VirtualMachine.IDSizes` (the mandatory first command) and wrap
    /// the client with the reported sizes.
    pub async fn new(client: Arc<JdwpClient>) -> Result<Vm, JdwpError> {
        use commands::virtual_machine as vm;
        let payload = client.command(vm::SET, vm::ID_SIZES, Vec::new()).await?;
        let sizes = codec::parse_id_sizes(&payload)?;
        Ok(Vm { client, sizes })
    }

    async fn cmd(&self, set: u8, cmd: u8, w: Writer) -> Result<Vec<u8>, JdwpError> {
        self.client.command(set, cmd, w.into_bytes()).await
    }

    pub async fn resume(&self) -> Result<(), JdwpError> {
        use commands::virtual_machine as vm;
        self.cmd(vm::SET, vm::RESUME, Writer::new()).await?;
        Ok(())
    }

    pub async fn suspend(&self) -> Result<(), JdwpError> {
        use commands::virtual_machine as vm;
        self.cmd(vm::SET, vm::SUSPEND, Writer::new()).await?;
        Ok(())
    }

    pub async fn exit(&self, code: u32) -> Result<(), JdwpError> {
        use commands::virtual_machine as vm;
        let mut w = Writer::new();
        w.u32(code);
        self.cmd(vm::SET, vm::EXIT, w).await?;
        Ok(())
    }

    pub async fn dispose(&self) -> Result<(), JdwpError> {
        use commands::virtual_machine as vm;
        self.cmd(vm::SET, vm::DISPOSE, Writer::new()).await?;
        Ok(())
    }

    pub async fn all_threads(&self) -> Result<Vec<u64>, JdwpError> {
        use commands::virtual_machine as vm;
        let payload = self.cmd(vm::SET, vm::ALL_THREADS, Writer::new()).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut threads = Vec::new();
        for _ in 0..count {
            threads.push(r.id(self.sizes.object)?);
        }
        Ok(threads)
    }

    pub async fn all_classes(&self) -> Result<Vec<ClassEntry>, JdwpError> {
        use commands::virtual_machine as vm;
        let payload = self.cmd(vm::SET, vm::ALL_CLASSES, Writer::new()).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut classes = Vec::new();
        for _ in 0..count {
            classes.push(ClassEntry {
                type_tag: r.u8()?,
                type_id: r.id(self.sizes.reference_type)?,
                signature: r.string()?,
                status: r.u32()?,
            });
        }
        Ok(classes)
    }

    pub async fn classes_by_signature(&self, sig: &str) -> Result<Vec<ClassEntry>, JdwpError> {
        use commands::virtual_machine as vm;
        let mut w = Writer::new();
        w.string(sig);
        let payload = self.cmd(vm::SET, vm::CLASSES_BY_SIGNATURE, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut classes = Vec::new();
        for _ in 0..count {
            classes.push(ClassEntry {
                type_tag: r.u8()?,
                type_id: r.id(self.sizes.reference_type)?,
                signature: sig.to_string(),
                status: r.u32()?,
            });
        }
        Ok(classes)
    }

    pub async fn type_signature(&self, type_id: u64) -> Result<String, JdwpError> {
        use commands::reference_type as rt;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, type_id);
        let payload = self.cmd(rt::SET, rt::SIGNATURE, w).await?;
        Ok(Reader::new(&payload).string()?)
    }

    pub async fn source_file(&self, type_id: u64) -> Result<String, JdwpError> {
        use commands::reference_type as rt;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, type_id);
        let payload = self.cmd(rt::SET, rt::SOURCE_FILE, w).await?;
        Ok(Reader::new(&payload).string()?)
    }

    pub async fn methods(&self, type_id: u64) -> Result<Vec<MethodEntry>, JdwpError> {
        use commands::reference_type as rt;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, type_id);
        let payload = self.cmd(rt::SET, rt::METHODS, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut methods = Vec::new();
        for _ in 0..count {
            methods.push(MethodEntry {
                id: r.id(self.sizes.method)?,
                name: r.string()?,
                signature: r.string()?,
                mod_bits: r.u32()?,
            });
        }
        Ok(methods)
    }

    pub async fn fields(&self, type_id: u64) -> Result<Vec<FieldEntry>, JdwpError> {
        use commands::reference_type as rt;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, type_id);
        let payload = self.cmd(rt::SET, rt::FIELDS, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut fields = Vec::new();
        for _ in 0..count {
            fields.push(FieldEntry {
                id: r.id(self.sizes.field)?,
                name: r.string()?,
                signature: r.string()?,
                mod_bits: r.u32()?,
            });
        }
        Ok(fields)
    }

    pub async fn superclass(&self, class_id: u64) -> Result<u64, JdwpError> {
        use commands::class_type as ct;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, class_id);
        let payload = self.cmd(ct::SET, ct::SUPERCLASS, w).await?;
        Reader::new(&payload)
            .id(self.sizes.reference_type)
            .map_err(Into::into)
    }

    pub async fn line_table(&self, class_id: u64, method_id: u64) -> Result<LineTable, JdwpError> {
        use commands::method as m;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, class_id);
        w.id(self.sizes.method, method_id);
        let payload = self.cmd(m::SET, m::LINE_TABLE, w).await?;
        let mut r = Reader::new(&payload);
        let start = r.u64()? as i64;
        let end = r.u64()? as i64;
        let count = r.u32()?;
        let mut lines = Vec::new();
        for _ in 0..count {
            let index = r.u64()?;
            let line = r.u32()?;
            lines.push((index, line));
        }
        Ok(LineTable { start, end, lines })
    }

    pub async fn variable_table(
        &self,
        class_id: u64,
        method_id: u64,
    ) -> Result<Vec<VariableSlot>, JdwpError> {
        use commands::method as m;
        let mut w = Writer::new();
        w.id(self.sizes.reference_type, class_id);
        w.id(self.sizes.method, method_id);
        let payload = self.cmd(m::SET, m::VARIABLE_TABLE, w).await?;
        let mut r = Reader::new(&payload);
        let _arg_cnt = r.u32()?;
        let count = r.u32()?;
        let mut slots = Vec::new();
        for _ in 0..count {
            slots.push(VariableSlot {
                code_index: r.u64()?,
                name: r.string()?,
                signature: r.string()?,
                length: r.u32()?,
                slot: r.u32()?,
            });
        }
        Ok(slots)
    }

    pub async fn thread_name(&self, thread: u64) -> Result<String, JdwpError> {
        use commands::thread_reference as tr;
        let mut w = Writer::new();
        w.id(self.sizes.object, thread);
        let payload = self.cmd(tr::SET, tr::NAME, w).await?;
        Ok(Reader::new(&payload).string()?)
    }

    pub async fn frame_count(&self, thread: u64) -> Result<u32, JdwpError> {
        use commands::thread_reference as tr;
        let mut w = Writer::new();
        w.id(self.sizes.object, thread);
        let payload = self.cmd(tr::SET, tr::FRAME_COUNT, w).await?;
        Reader::new(&payload).u32().map_err(Into::into)
    }

    /// `length == -1` means "all remaining frames".
    pub async fn frames(
        &self,
        thread: u64,
        start: u32,
        length: i32,
    ) -> Result<Vec<FrameEntry>, JdwpError> {
        use commands::thread_reference as tr;
        let mut w = Writer::new();
        w.id(self.sizes.object, thread);
        w.u32(start);
        w.u32(length as u32);
        let payload = self.cmd(tr::SET, tr::FRAMES, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut frames = Vec::new();
        for _ in 0..count {
            frames.push(FrameEntry {
                frame_id: r.id(self.sizes.frame)?,
                location: r.location(self.sizes)?,
            });
        }
        Ok(frames)
    }

    /// `StackFrame.GetValues` — `slots` are `(slot index, expected tag)`.
    pub async fn stack_values(
        &self,
        thread: u64,
        frame: u64,
        slots: &[(u32, u8)],
    ) -> Result<Vec<Value>, JdwpError> {
        use commands::stack_frame as sf;
        let mut w = Writer::new();
        w.id(self.sizes.object, thread);
        w.id(self.sizes.frame, frame);
        w.u32(slots.len() as u32);
        for &(slot, tag) in slots {
            w.u32(slot);
            w.u8(tag);
        }
        let payload = self.cmd(sf::SET, sf::GET_VALUES, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(r.tagged_value(self.sizes)?);
        }
        Ok(values)
    }

    pub async fn this_object(&self, thread: u64, frame: u64) -> Result<Value, JdwpError> {
        use commands::stack_frame as sf;
        let mut w = Writer::new();
        w.id(self.sizes.object, thread);
        w.id(self.sizes.frame, frame);
        let payload = self.cmd(sf::SET, sf::THIS_OBJECT, w).await?;
        Reader::new(&payload)
            .tagged_value(self.sizes)
            .map_err(Into::into)
    }

    /// `ObjectReference.ReferenceType` — the runtime type of an object.
    pub async fn object_type(&self, object: u64) -> Result<(u8, u64), JdwpError> {
        use commands::object_reference as or;
        let mut w = Writer::new();
        w.id(self.sizes.object, object);
        let payload = self.cmd(or::SET, or::REFERENCE_TYPE, w).await?;
        let mut r = Reader::new(&payload);
        let tag = r.u8()?;
        let type_id = r.id(self.sizes.reference_type)?;
        Ok((tag, type_id))
    }

    pub async fn object_values(
        &self,
        object: u64,
        field_ids: &[u64],
    ) -> Result<Vec<Value>, JdwpError> {
        use commands::object_reference as or;
        let mut w = Writer::new();
        w.id(self.sizes.object, object);
        w.u32(field_ids.len() as u32);
        for &f in field_ids {
            w.id(self.sizes.field, f);
        }
        let payload = self.cmd(or::SET, or::GET_VALUES, w).await?;
        let mut r = Reader::new(&payload);
        let count = r.u32()?;
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(r.tagged_value(self.sizes)?);
        }
        Ok(values)
    }

    pub async fn string_value(&self, object: u64) -> Result<String, JdwpError> {
        use commands::string_reference as sr;
        let mut w = Writer::new();
        w.id(self.sizes.object, object);
        let payload = self.cmd(sr::SET, sr::VALUE, w).await?;
        Ok(Reader::new(&payload).value_string()?)
    }

    pub async fn array_length(&self, object: u64) -> Result<u32, JdwpError> {
        use commands::array_reference as ar;
        let mut w = Writer::new();
        w.id(self.sizes.object, object);
        let payload = self.cmd(ar::SET, ar::LENGTH, w).await?;
        let mut r = Reader::new(&payload);
        Ok((r.u32()? as i32).max(0) as u32)
    }

    /// `ArrayReference.GetValues` — an arrayregion: primitive element
    /// values arrive untagged, object elements tagged.
    pub async fn array_values(
        &self,
        object: u64,
        first: u32,
        length: u32,
    ) -> Result<Vec<Value>, JdwpError> {
        use commands::array_reference as ar;
        let mut w = Writer::new();
        w.id(self.sizes.object, object);
        w.u32(first);
        w.u32(length);
        let payload = self.cmd(ar::SET, ar::GET_VALUES, w).await?;
        let mut r = Reader::new(&payload);
        let region_tag = r.u8()?;
        let count = r.u32()?;
        let object_family = matches!(
            region_tag,
            commands::tag::OBJECT
                | commands::tag::STRING
                | commands::tag::ARRAY
                | commands::tag::THREAD
                | commands::tag::THREAD_GROUP
                | commands::tag::CLASS_LOADER
                | commands::tag::CLASS_OBJECT
        );
        let mut values = Vec::new();
        for _ in 0..count {
            let v = if object_family {
                r.tagged_value(self.sizes)?
            } else {
                r.untagged_value(region_tag, self.sizes)?
            };
            values.push(v);
        }
        Ok(values)
    }

    /// `EventRequest.Set` → the new request id.
    pub async fn event_request_set(
        &self,
        event_kind: u8,
        suspend_policy: u8,
        modifiers: &[Modifier],
    ) -> Result<u32, JdwpError> {
        use commands::event_request as er;
        use commands::modifier as m;
        let mut w = Writer::new();
        w.u8(event_kind);
        w.u8(suspend_policy);
        w.u32(modifiers.len() as u32);
        for modifier in modifiers {
            match modifier {
                Modifier::Count(n) => {
                    w.u8(m::COUNT).u32(*n);
                }
                Modifier::ClassMatch(pattern) => {
                    w.u8(m::CLASS_MATCH).string(pattern);
                }
                Modifier::LocationOnly(loc) => {
                    w.u8(m::LOCATION_ONLY).location(self.sizes, *loc);
                }
                Modifier::ExceptionOnly { caught, uncaught } => {
                    w.u8(m::EXCEPTION_ONLY);
                    w.id(self.sizes.reference_type, 0); // all exception types
                    w.u8(u8::from(*caught));
                    w.u8(u8::from(*uncaught));
                }
                Modifier::Step {
                    thread,
                    size,
                    depth,
                } => {
                    w.u8(m::STEP);
                    w.id(self.sizes.object, *thread);
                    w.u32(*size);
                    w.u32(*depth);
                }
            }
        }
        let payload = self.cmd(er::SET, er::CMD_SET, w).await?;
        Reader::new(&payload).u32().map_err(Into::into)
    }

    pub async fn event_request_clear(
        &self,
        event_kind: u8,
        request_id: u32,
    ) -> Result<(), JdwpError> {
        use commands::event_request as er;
        let mut w = Writer::new();
        w.u8(event_kind);
        w.u32(request_id);
        self.cmd(er::SET, er::CMD_CLEAR, w).await?;
        Ok(())
    }
}
