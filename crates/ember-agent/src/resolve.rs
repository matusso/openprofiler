//! jmethodID → [`ember_model::Frame`] resolution with caching.

use crate::jvm::{Jvmti, jvmti};
use ember_model::symbols::java_class_name;
use ember_model::{FrameId, FrameKind, MethodId, Symbols};
use jvmti_sys::*;
use std::collections::HashMap;
use std::ptr;

struct MethodEntry {
    method: MethodId,
    /// `(start bci, line)` sorted by bci.
    lines: Vec<(jlocation, i32)>,
    native: bool,
}

#[derive(Default)]
pub struct Resolver {
    methods: HashMap<usize, MethodEntry>,
    classes: HashMap<String, String>,
    pub line_numbers: bool,
}

/// Converts a JVM type signature (`Ljava/lang/String;`, `[I`) to a Java class name.
pub fn signature_to_class(sig: &str) -> String {
    match sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        Some(inner) => java_class_name(inner),
        None => java_class_name(sig),
    }
}

impl Resolver {
    fn entry(&mut self, jvmti: Jvmti, symbols: &mut Symbols, mid: jmethodID) -> &MethodEntry {
        let key = mid as usize;
        if !self.methods.contains_key(&key) {
            let e = self.load(jvmti, symbols, mid);
            self.methods.insert(key, e);
        }
        &self.methods[&key]
    }

    fn load(&mut self, jvmti: Jvmti, symbols: &mut Symbols, mid: jmethodID) -> MethodEntry {
        let unknown = |symbols: &mut Symbols| MethodEntry {
            method: symbols.intern_method("", "[unknown method]", ""),
            lines: vec![],
            native: false,
        };
        if mid.is_null() {
            return unknown(symbols);
        }
        let (mut name, mut sig) = (ptr::null_mut(), ptr::null_mut());
        if jvmti!(jvmti, GetMethodName, mid, &mut name, &mut sig, ptr::null_mut()) != JVMTI_ERROR_NONE {
            return unknown(symbols);
        }
        let name = jvmti.take_string(name);
        let sig = jvmti.take_string(sig);
        let mut klass: jclass = ptr::null_mut();
        let class_name = if jvmti!(jvmti, GetMethodDeclaringClass, mid, &mut klass) == JVMTI_ERROR_NONE {
            let csig = jvmti.class_signature(klass).unwrap_or_default();
            self.classes.entry(csig.clone()).or_insert_with(|| signature_to_class(&csig)).clone()
        } else {
            String::new()
        };
        // GetMethodDeclaringClass returns a local reference in the calling thread's current JNI
        // frame; callers run resolution inside a pushed local frame.
        let mut native: jboolean = 0;
        jvmti!(jvmti, IsMethodNative, mid, &mut native);
        let mut lines = vec![];
        if self.line_numbers && native == 0 {
            let mut count = 0;
            let mut table: *mut jvmtiLineNumberEntry = ptr::null_mut();
            if jvmti!(jvmti, GetLineNumberTable, mid, &mut count, &mut table) == JVMTI_ERROR_NONE
                && !table.is_null()
                && count > 0
            {
                // SAFETY: JVMTI returned `count` valid entries.
                let entries = unsafe { std::slice::from_raw_parts(table, count.max(0) as usize) };
                lines = entries.iter().map(|e| (e.start_location, e.line_number)).collect();
                lines.sort_unstable();
                jvmti.dealloc(table);
            }
        }
        MethodEntry { method: symbols.intern_method(&class_name, &name, &sig), lines, native: native != 0 }
    }

    /// Resolves a frame. `bci < 0` means unknown/native. `kind` is the caller's best knowledge of
    /// the frame type; native methods are always reported as [`FrameKind::Native`].
    pub fn frame(
        &mut self,
        jvmti: Jvmti,
        symbols: &mut Symbols,
        mid: jmethodID,
        bci: i64,
        kind: FrameKind,
    ) -> FrameId {
        let e = self.entry(jvmti, symbols, mid);
        let line = if bci >= 0 {
            match e.lines.binary_search_by(|(start, _)| start.cmp(&bci)) {
                Ok(i) => e.lines[i].1,
                Err(0) => -1,
                Err(i) => e.lines[i - 1].1,
            }
        } else {
            -1
        };
        let kind = if e.native { FrameKind::Native } else { kind };
        let method = e.method;
        symbols.intern_frame(method, line, kind)
    }

    pub fn class_name(&mut self, jvmti: Jvmti, klass: jclass) -> String {
        let sig = jvmti.class_signature(klass).unwrap_or_default();
        self.classes.entry(sig.clone()).or_insert_with(|| signature_to_class(&sig)).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        assert_eq!(signature_to_class("Ljava/lang/String;"), "java.lang.String");
        assert_eq!(signature_to_class("[Ljava/lang/Object;"), "java.lang.Object[]");
        assert_eq!(signature_to_class("[J"), "long[]");
    }
}
