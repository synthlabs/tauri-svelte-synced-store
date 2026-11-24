use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::io::{self, Write};

use crate::protocol::{JsonPatchOp, SyncError};

struct CountingWriter(usize);

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn json_byte_len<T: Serialize>(value: &T) -> Result<usize, SyncError> {
    let mut counter = CountingWriter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|err| SyncError::SerializeFailed(err.to_string()))?;
    Ok(counter.0)
}

pub fn escape_pointer_segment(segment: &str) -> Cow<'_, str> {
    if !segment.contains(['~', '/']) {
        return Cow::Borrowed(segment);
    }

    let mut out = String::with_capacity(segment.len() + 4);
    for ch in segment.chars() {
        match ch {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

fn append_pointer(parent: &str, segment: &str) -> String {
    let escaped = escape_pointer_segment(segment);
    let mut out = String::with_capacity(parent.len() + 1 + escaped.len());
    out.push_str(parent);
    out.push('/');
    out.push_str(&escaped);
    out
}

fn unescape_pointer_segment(segment: &str) -> Result<String, SyncError> {
    if !segment.contains('~') {
        return Ok(segment.to_owned());
    }

    let mut out = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => {
                    return Err(SyncError::InvalidPatch(format!(
                        "invalid escape sequence in JSON Pointer segment: {segment}"
                    )));
                }
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

pub fn parse_pointer(path: &str) -> Result<Vec<String>, SyncError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        return Err(SyncError::InvalidPatch(format!(
            "JSON Pointer must be empty or start with '/': {path}"
        )));
    }

    path[1..].split('/').map(unescape_pointer_segment).collect()
}

/// Computes an RFC 6902 patch transforming `old` into `new`.
/// Nested JSON objects are diffed recursively; arrays and primitives are replaced atomically.
pub fn diff_json(old: &Value, new: &Value) -> Vec<JsonPatchOp> {
    let mut ops = Vec::new();
    diff_into("", old, new, &mut ops);
    ops
}

fn diff_into(path: &str, old: &Value, new: &Value, ops: &mut Vec<JsonPatchOp>) {
    if let (Value::Object(old_map), Value::Object(new_map)) = (old, new) {
        for key in old_map.keys() {
            if !new_map.contains_key(key) {
                ops.push(JsonPatchOp::Remove {
                    path: append_pointer(path, key),
                });
            }
        }
        for (key, new_val) in new_map {
            let child_path = append_pointer(path, key);
            match old_map.get(key) {
                Some(old_val) => diff_into(&child_path, old_val, new_val, ops),
                None => ops.push(JsonPatchOp::Add {
                    path: child_path,
                    value: new_val.clone(),
                }),
            }
        }
        return;
    }

    if old != new {
        ops.push(JsonPatchOp::Replace {
            path: path.to_owned(),
            value: new.clone(),
        });
    }
}

fn resolve_parent_object<'a>(
    root: &'a mut Value,
    parent_segments: &[String],
    full_path: &str,
) -> Result<&'a mut serde_json::Map<String, Value>, SyncError> {
    let mut current = root;
    for segment in parent_segments {
        let Some(map) = current.as_object_mut() else {
            return Err(SyncError::InvalidPatch(format!(
                "non-object parent along path '{full_path}'"
            )));
        };
        current = map.get_mut(segment).ok_or_else(|| {
            SyncError::InvalidPatch(format!(
                "missing parent segment '{segment}' along path '{full_path}'"
            ))
        })?;
    }

    current.as_object_mut().ok_or_else(|| {
        SyncError::InvalidPatch(format!("target parent is not an object for '{full_path}'"))
    })
}

/// Applies RFC 6902 object-level patch operations to a clone of `base`.
pub fn apply_patch(base: &Value, ops: &[JsonPatchOp]) -> Result<Value, SyncError> {
    let mut target = base.clone();

    for op in ops {
        match op {
            JsonPatchOp::Add { path, value } => {
                let segments = parse_pointer(path)?;
                if let Some((last, parent_segments)) = segments.split_last() {
                    let parent = resolve_parent_object(&mut target, parent_segments, path)?;
                    parent.insert(last.clone(), value.clone());
                } else {
                    target = value.clone();
                }
            }
            JsonPatchOp::Remove { path } => {
                let segments = parse_pointer(path)?;
                let Some((last, parent_segments)) = segments.split_last() else {
                    return Err(SyncError::InvalidPatch(
                        "cannot remove root document".to_owned(),
                    ));
                };
                let parent = resolve_parent_object(&mut target, parent_segments, path)?;
                if parent.remove(last).is_none() {
                    return Err(SyncError::InvalidPatch(format!(
                        "cannot remove missing path '{path}'"
                    )));
                }
            }
            JsonPatchOp::Replace { path, value } => {
                let segments = parse_pointer(path)?;
                if let Some((last, parent_segments)) = segments.split_last() {
                    let parent = resolve_parent_object(&mut target, parent_segments, path)?;
                    let Some(slot) = parent.get_mut(last) else {
                        return Err(SyncError::InvalidPatch(format!(
                            "cannot replace missing path '{path}'"
                        )));
                    };
                    *slot = value.clone();
                } else {
                    target = value.clone();
                }
            }
        }
    }

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pointer_escaping_handles_tilde_slash_and_empty_key() {
        let old = json!({
            "": 1,
            "a/b": { "c~d": 2, "keep": true },
            "~01": "old"
        });
        let new = json!({
            "": 2,
            "a/b": { "c~d": 3, "keep": true },
            "~01": "new"
        });

        let ops = diff_json(&old, &new);
        assert_eq!(
            ops,
            vec![
                JsonPatchOp::Replace {
                    path: "/".into(),
                    value: json!(2),
                },
                JsonPatchOp::Replace {
                    path: "/a~1b/c~0d".into(),
                    value: json!(3),
                },
                JsonPatchOp::Replace {
                    path: "/~001".into(),
                    value: json!("new"),
                },
            ]
        );
        assert_eq!(apply_patch(&old, &ops).unwrap(), new);
    }

    #[test]
    fn diff_and_apply_round_trip_nested_objects_options_and_arrays() {
        let old = json!({
            "nested": { "a": 1, "b": null, "removed": "bye" },
            "items": [1, 2, 3],
            "unchanged": "same"
        });
        let new = json!({
            "nested": { "a": 1, "b": "present", "added": null },
            "items": [1, 2, 4],
            "unchanged": "same"
        });

        let ops = diff_json(&old, &new);
        assert_eq!(
            ops,
            vec![
                JsonPatchOp::Replace {
                    path: "/items".into(),
                    value: json!([1, 2, 4]),
                },
                JsonPatchOp::Remove {
                    path: "/nested/removed".into(),
                },
                JsonPatchOp::Add {
                    path: "/nested/added".into(),
                    value: json!(null),
                },
                JsonPatchOp::Replace {
                    path: "/nested/b".into(),
                    value: json!("present"),
                },
            ]
        );
        assert_eq!(apply_patch(&old, &ops).unwrap(), new);
    }

    #[test]
    fn diff_empty_when_values_match_and_handles_root_replace() {
        let value = json!({ "a": [1, 2], "b": { "c": true } });
        assert!(diff_json(&value, &value).is_empty());

        let root_ops = diff_json(&json!(1), &json!({ "x": 2 }));
        assert_eq!(
            root_ops,
            vec![JsonPatchOp::Replace {
                path: "".into(),
                value: json!({ "x": 2 }),
            }]
        );
        assert_eq!(
            apply_patch(&json!(1), &root_ops).unwrap(),
            json!({ "x": 2 })
        );
    }

    #[test]
    fn apply_patch_rejects_invalid_paths_and_missing_targets() {
        let base = json!({ "a": 1, "arr": [1, 2] });

        for invalid in [
            vec![JsonPatchOp::Replace {
                path: "no-leading-slash".into(),
                value: json!(2),
            }],
            vec![JsonPatchOp::Replace {
                path: "/bad~2escape".into(),
                value: json!(2),
            }],
            vec![JsonPatchOp::Remove { path: "".into() }],
            vec![JsonPatchOp::Remove {
                path: "/missing".into(),
            }],
            vec![JsonPatchOp::Replace {
                path: "/missing".into(),
                value: json!(2),
            }],
            vec![JsonPatchOp::Add {
                path: "/missing_parent/child".into(),
                value: json!(2),
            }],
            vec![JsonPatchOp::Add {
                path: "/arr/0".into(),
                value: json!(9),
            }],
        ] {
            assert!(
                matches!(
                    apply_patch(&base, &invalid),
                    Err(SyncError::InvalidPatch(_))
                ),
                "expected InvalidPatch for {invalid:?}"
            );
        }
    }

    #[test]
    fn json_byte_len_matches_serde_json_to_vec() {
        let val =
            json!({ "count": 42, "nested": { "msg": "hello \"world\" 🚀" }, "arr": [1, 2, 3] });
        let expected = serde_json::to_vec(&val).unwrap().len();
        assert_eq!(json_byte_len(&val).unwrap(), expected);
    }
}
