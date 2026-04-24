//! Garbage-collected heap objects with reference counting.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;

use crate::intern::InternedString;
use crate::value::Value;

/// Discriminant for heap object types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjType {
    String,
    Array,
    Tuple,
    Dictionary,
    Closure,
    NativeFn,
    Instance,
    Enum,
    TypeDef,
    Module,
    Upvalue,
    Error,
}

/// Header prepended to every heap-allocated object.
#[repr(C)]
pub struct GcHeader {
    pub rc: Cell<u32>,
    pub obj_type: ObjType,
}

/// A heap-allocated object. The GcHeader is stored inline.
#[repr(C)]
pub struct Object {
    pub header: GcHeader,
    pub data: ObjectData,
}

pub enum ObjectData {
    String(FaiString),
    Array(FaiArray),
    Tuple(FaiTuple),
    Dictionary(FaiDictionary),
    Closure(FaiClosure),
    NativeFn(FaiNativeFn),
    Instance(FaiInstance),
    Enum(FaiEnum),
    TypeDef(FaiTypeDef),
    Module(FaiModule),
    Upvalue(FaiUpvalue),
    Error(FaiError),
}

// ── Object types ───────────────────────────────────────────────────

pub struct FaiString {
    pub hash: u32,
    pub data: String,
}

pub struct FaiArray {
    pub items: Vec<Value>,
}

pub struct FaiTuple {
    pub items: Vec<Value>,
}

pub struct FaiDictionary {
    pub entries: Vec<(String, Value)>,
}

pub struct FaiClosure {
    pub proto_index: u32,
    pub upvalues: Vec<GcRef>,
}

pub type NativeFnPtr = fn(&mut dyn crate::platform::Platform, &[Value]) -> Result<Value, FaiError>;

pub struct FaiNativeFn {
    pub name: InternedString,
    pub func: NativeFnPtr,
}

pub struct FaiInstance {
    pub type_name: InternedString,
    pub fields: Vec<(InternedString, Value)>,
}

pub struct FaiEnum {
    pub name: InternedString,
    pub members: Vec<InternedString>,
}

pub struct FaiTypeDef {
    pub name: InternedString,
    pub field_names: Vec<InternedString>,
    pub defaults: Vec<Option<Value>>,
}

pub struct FaiModule {
    pub name: InternedString,
    pub exports: HashMap<InternedString, Value>,
}

pub struct FaiUpvalue {
    pub location: UpvalueLocation,
}

pub enum UpvalueLocation {
    /// Points to a register on the stack (index into the current call frame's slots).
    Open(u32),
    /// Value has been captured (scope exited).
    Closed(Value),
}

pub struct FaiError {
    pub message: String,
    pub kind: Option<String>,
}

// ── GcRef: a non-null pointer to a heap Object ────────────────────

#[derive(Clone, Copy)]
pub struct GcRef {
    ptr: *mut Object,
}

impl GcRef {
    /// # Safety
    /// The pointer must be non-null and point to a valid Object.
    #[inline(always)]
    pub unsafe fn from_ptr(ptr: *mut Object) -> Self {
        Self { ptr }
    }

    #[inline(always)]
    pub fn as_ptr(self) -> *mut Object {
        self.ptr
    }

    #[inline]
    pub fn obj(&self) -> &Object {
        unsafe { &*self.ptr }
    }

    #[inline]
    pub fn obj_mut(&self) -> &mut Object {
        unsafe { &mut *self.ptr }
    }

    #[inline]
    pub fn data(&self) -> &ObjectData {
        &self.obj().data
    }

    #[inline]
    pub fn data_mut(&self) -> &mut ObjectData {
        &mut self.obj_mut().data
    }

    #[inline]
    pub fn obj_type(&self) -> ObjType {
        self.obj().header.obj_type
    }
}

impl fmt::Debug for GcRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GcRef({:p})", self.ptr)
    }
}

// ── Heap: allocator + ref count management ────────────────────────

pub struct Heap {
    /// All live objects, for sweep/cleanup.
    objects: Vec<*mut Object>,
}

impl Heap {
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
        }
    }

    pub fn alloc(&mut self, obj_type: ObjType, data: ObjectData) -> GcRef {
        let obj = Box::new(Object {
            header: GcHeader {
                rc: Cell::new(1),
                obj_type,
            },
            data,
        });
        let ptr = Box::into_raw(obj);
        self.objects.push(ptr);
        unsafe { GcRef::from_ptr(ptr) }
    }

    pub fn alloc_string(&mut self, s: String) -> GcRef {
        let hash = fxhash(&s);
        self.alloc(
            ObjType::String,
            ObjectData::String(FaiString { hash, data: s }),
        )
    }

    pub fn alloc_array(&mut self, items: Vec<Value>) -> GcRef {
        self.alloc(ObjType::Array, ObjectData::Array(FaiArray { items }))
    }

    pub fn alloc_tuple(&mut self, items: Vec<Value>) -> GcRef {
        self.alloc(ObjType::Tuple, ObjectData::Tuple(FaiTuple { items }))
    }

    pub fn alloc_dictionary(&mut self, entries: Vec<(String, Value)>) -> GcRef {
        self.alloc(
            ObjType::Dictionary,
            ObjectData::Dictionary(FaiDictionary { entries }),
        )
    }

    pub fn alloc_closure(&mut self, proto_index: u32, upvalues: Vec<GcRef>) -> GcRef {
        self.alloc(
            ObjType::Closure,
            ObjectData::Closure(FaiClosure {
                proto_index,
                upvalues,
            }),
        )
    }

    pub fn alloc_error(&mut self, message: String) -> GcRef {
        self.alloc(
            ObjType::Error,
            ObjectData::Error(FaiError {
                message,
                kind: None,
            }),
        )
    }

    pub fn alloc_instance(
        &mut self,
        type_name: InternedString,
        fields: Vec<(InternedString, Value)>,
    ) -> GcRef {
        self.alloc(
            ObjType::Instance,
            ObjectData::Instance(FaiInstance { type_name, fields }),
        )
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        for ptr in &self.objects {
            unsafe {
                drop(Box::from_raw(*ptr));
            }
        }
    }
}

/// Simple FNV-1a-style hash for strings.
fn fxhash(s: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}
