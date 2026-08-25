//! JDWP command-set/command numbers, event kinds, modifier kinds, and error
//! codes — centralized so every wire number in the crate traces to one place.
//! Numbers are from the JDK 21 JDWP specification
//! (<https://docs.oracle.com/en/java/javase/21/docs/specs/jdwp/jdwp-protocol.html>).

/// VirtualMachine command set (1).
pub mod virtual_machine {
    pub const SET: u8 = 1;
    pub const VERSION: u8 = 1;
    pub const CLASSES_BY_SIGNATURE: u8 = 2;
    pub const ALL_CLASSES: u8 = 3;
    pub const ALL_THREADS: u8 = 4;
    pub const DISPOSE: u8 = 6;
    pub const ID_SIZES: u8 = 7;
    pub const SUSPEND: u8 = 8;
    pub const RESUME: u8 = 9;
    pub const EXIT: u8 = 10;
}

/// ReferenceType command set (2).
pub mod reference_type {
    pub const SET: u8 = 2;
    pub const SIGNATURE: u8 = 1;
    pub const FIELDS: u8 = 4;
    pub const METHODS: u8 = 5;
    pub const SOURCE_FILE: u8 = 7;
}

/// ClassType command set (3).
pub mod class_type {
    pub const SET: u8 = 3;
    pub const SUPERCLASS: u8 = 1;
}

/// Method command set (6).
pub mod method {
    pub const SET: u8 = 6;
    pub const LINE_TABLE: u8 = 1;
    pub const VARIABLE_TABLE: u8 = 2;
}

/// ObjectReference command set (9).
pub mod object_reference {
    pub const SET: u8 = 9;
    pub const REFERENCE_TYPE: u8 = 1;
    pub const GET_VALUES: u8 = 2;
}

/// StringReference command set (10).
pub mod string_reference {
    pub const SET: u8 = 10;
    pub const VALUE: u8 = 1;
}

/// ThreadReference command set (11).
pub mod thread_reference {
    pub const SET: u8 = 11;
    pub const NAME: u8 = 1;
    pub const FRAMES: u8 = 6;
    pub const FRAME_COUNT: u8 = 7;
}

/// ArrayReference command set (13).
pub mod array_reference {
    pub const SET: u8 = 13;
    pub const LENGTH: u8 = 1;
    pub const GET_VALUES: u8 = 2;
}

/// EventRequest command set (15).
pub mod event_request {
    pub const SET: u8 = 15;
    pub const CMD_SET: u8 = 1;
    pub const CMD_CLEAR: u8 = 2;
}

/// StackFrame command set (16).
pub mod stack_frame {
    pub const SET: u8 = 16;
    pub const GET_VALUES: u8 = 1;
    pub const THIS_OBJECT: u8 = 3;
}

/// Event command set (64) — VM→debugger Composite events.
pub mod event {
    pub const SET: u8 = 64;
    pub const COMPOSITE: u8 = 100;
}

/// EventKind constants.
pub mod event_kind {
    pub const SINGLE_STEP: u8 = 1;
    pub const BREAKPOINT: u8 = 2;
    pub const EXCEPTION: u8 = 4;
    pub const CLASS_PREPARE: u8 = 8;
    pub const VM_START: u8 = 90;
    pub const VM_DEATH: u8 = 99;
}

/// EventRequest modifier kinds.
pub mod modifier {
    pub const COUNT: u8 = 1;
    pub const CLASS_MATCH: u8 = 5;
    pub const LOCATION_ONLY: u8 = 7;
    pub const EXCEPTION_ONLY: u8 = 8;
    pub const STEP: u8 = 10;
}

/// SuspendPolicy constants.
pub mod suspend_policy {
    pub const NONE: u8 = 0;
    pub const ALL: u8 = 2;
}

/// Step modifier size/depth constants.
pub mod step {
    pub const SIZE_LINE: u32 = 1;
    pub const DEPTH_INTO: u32 = 0;
    pub const DEPTH_OVER: u32 = 1;
    pub const DEPTH_OUT: u32 = 2;
}

/// JDWP error codes this adapter treats specially.
pub mod error {
    /// Line/variable debug information was not compiled in (`javac` without
    /// `-g`) — an expected, non-fatal reply for `LineTable`/`VariableTable`.
    pub const ABSENT_INFORMATION: u16 = 101;
}

/// Value type tags (JDWP `Tag` constants — ASCII of the JNI signature char).
pub mod tag {
    pub const ARRAY: u8 = b'[';
    pub const BYTE: u8 = b'B';
    pub const CHAR: u8 = b'C';
    pub const OBJECT: u8 = b'L';
    pub const FLOAT: u8 = b'F';
    pub const DOUBLE: u8 = b'D';
    pub const INT: u8 = b'I';
    pub const LONG: u8 = b'J';
    pub const SHORT: u8 = b'S';
    pub const VOID: u8 = b'V';
    pub const BOOLEAN: u8 = b'Z';
    pub const STRING: u8 = b's';
    pub const THREAD: u8 = b't';
    pub const THREAD_GROUP: u8 = b'g';
    pub const CLASS_LOADER: u8 = b'l';
    pub const CLASS_OBJECT: u8 = b'c';
}
