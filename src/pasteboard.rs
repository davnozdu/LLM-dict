//! Полный снимок pasteboard. В другие потоки передаются только Rust-данные.
use anyhow::{bail, ensure, Result};
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    items: Vec<Vec<(String, Vec<u8>)>>,
}

impl Snapshot {
    pub fn capture() -> Result<Self> {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            Self::capture_from(pb)
        })
    }

    fn capture_from(pb: *mut AnyObject) -> Result<Self> {
        unsafe {
            ensure!(!pb.is_null(), "буфер обмена недоступен");
            let before: i64 = msg_send![pb, changeCount];
            let items: *mut AnyObject = msg_send![pb, pasteboardItems];
            let count: usize = if items.is_null() {
                0
            } else {
                msg_send![items, count]
            };
            let mut snapshot = Self { items: Vec::new() };
            let mut total = 0usize;
            for i in 0..count {
                let item: *mut AnyObject = msg_send![items, objectAtIndex: i];
                let types: *mut AnyObject = msg_send![item, types];
                ensure!(!types.is_null(), "не прочитать форматы буфера");
                let count: usize = msg_send![types, count];
                let mut representations = Vec::new();
                for j in 0..count {
                    let kind: *mut AnyObject = msg_send![types, objectAtIndex: j];
                    let name: *const std::ffi::c_char = msg_send![kind, UTF8String];
                    ensure!(!name.is_null(), "не прочитать имя формата буфера");
                    let name = std::ffi::CStr::from_ptr(name).to_str()?.to_owned();
                    let data: *mut AnyObject = msg_send![item, dataForType: kind];
                    ensure!(!data.is_null(), "не сохранить формат буфера {name}");
                    let len: usize = msg_send![data, length];
                    total = total
                        .checked_add(len)
                        .ok_or_else(|| anyhow::anyhow!("слишком большой буфер"))?;
                    // При слишком большом снимке отказываемся до изменения буфера.
                    ensure!(
                        total <= 128 * 1024 * 1024,
                        "буфер больше 128 МиБ; сохраните его содержимое перед вставкой"
                    );
                    let bytes: *const std::ffi::c_void = msg_send![data, bytes];
                    let bytes = if len == 0 {
                        Vec::new()
                    } else {
                        ensure!(!bytes.is_null(), "не прочитать данные буфера");
                        std::slice::from_raw_parts(bytes.cast::<u8>(), len).to_vec()
                    };
                    representations.push((name, bytes));
                }
                snapshot.items.push(representations);
            }
            let after: i64 = msg_send![pb, changeCount];
            ensure!(before == after, "буфер изменился во время сохранения");
            Ok(snapshot)
        }
    }

    pub fn restore(&self, expected: i64) -> Result<Option<i64>> {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
            self.restore_to(pb, Some(expected))
        })
    }

    fn restore_to(&self, pb: *mut AnyObject, expected: Option<i64>) -> Result<Option<i64>> {
        unsafe {
            let array: *mut AnyObject = msg_send![class!(NSMutableArray), new];
            let array = Retained::from_raw(array)
                .ok_or_else(|| anyhow::anyhow!("не создать снимок буфера"))?;
            // Готовим все представления до clearContents.
            for representations in &self.items {
                let item: *mut AnyObject = msg_send![class!(NSPasteboardItem), new];
                let item = Retained::from_raw(item)
                    .ok_or_else(|| anyhow::anyhow!("не создать элемент буфера"))?;
                for (kind, bytes) in representations {
                    let kind = NSString::from_str(kind);
                    let data: *mut AnyObject = msg_send![class!(NSData), dataWithBytes: bytes.as_ptr().cast::<std::ffi::c_void>(), length: bytes.len()];
                    ensure!(!data.is_null(), "не создать данные буфера");
                    let ok: bool = msg_send![&*item, setData: data, forType: &*kind];
                    ensure!(ok, "не восстановить формат буфера");
                }
                let _: () = msg_send![&*array, addObject: &*item];
            }
            ensure!(!pb.is_null(), "буфер обмена недоступен");
            let current: i64 = msg_send![pb, changeCount];
            if expected.is_some_and(|expected| expected != current) {
                return Ok(None);
            }
            let _: i64 = msg_send![pb, clearContents];
            if !self.items.is_empty() {
                let ok: bool = msg_send![pb, writeObjects: &*array];
                if !ok {
                    bail!("не восстановить содержимое буфера");
                }
            }
            Ok(Some(msg_send![pb, changeCount]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "нужен сервис pasteboard macOS; используется отдельный именованный буфер"]
    fn preserves_all_items_and_does_not_restore_over_new_copy() {
        autoreleasepool(|_| unsafe {
            let pb: *mut AnyObject = msg_send![class!(NSPasteboard), pasteboardWithUniqueName];
            assert!(!pb.is_null());
            let original = Snapshot {
                items: vec![
                    vec![
                        ("public.utf8-plain-text".into(), b"hello".to_vec()),
                        ("public.rtf".into(), b"{rtf}".to_vec()),
                    ],
                    vec![("public.png".into(), vec![137, 80, 78, 71, 0, 1, 2])],
                ],
            };
            let change = original.restore_to(pb, None).unwrap().unwrap();
            let captured = Snapshot::capture_from(pb).unwrap();
            assert_eq!(captured.items.len(), original.items.len());
            for (actual, expected) in captured.items.iter().zip(&original.items) {
                // macOS может добавить производные форматы, например UTF-16.
                for representation in expected {
                    assert!(actual.contains(representation));
                }
            }
            let empty = Snapshot { items: Vec::new() };
            empty.restore_to(pb, None).unwrap();
            assert_eq!(original.restore_to(pb, Some(change)).unwrap(), None);
            assert_eq!(Snapshot::capture_from(pb).unwrap(), empty);
            let _: () = msg_send![pb, releaseGlobally];
        });
    }
}
