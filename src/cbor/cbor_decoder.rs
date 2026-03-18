
use minicbor::{
    data::Tag,
    decode::{Decoder, ExtendedToken, ExtendedTokenizer, Token},
};
use serde_json::{Number, Value};

use crate::js_error::JsError;

/// Position info in the CBOR input (offset + length).
#[derive(Clone, Debug)]
pub struct CborPos {
    pub offset: usize,
    pub length: usize,
}

/// Collection types, including indefinite string/bytes *as arrays of chunks*.
#[derive(Clone, Debug)]
pub enum CborCollection {
    Tag(Option<Value>, Tag, CborPos, CborPos),
    Array(Value, Option<usize>, usize, CborPos, CborPos),
    Map(Value, Option<Value>, Option<usize>, usize, CborPos, CborPos),

    /// Stores a list of chunk tokens for an indefinite string.
    IndefiniteString(Vec<Value>, CborPos, CborPos),
    /// Stores a list of chunk tokens for an indefinite byte string.
    IndefiniteBytes(Vec<Value>, CborPos, CborPos),
}

impl CborCollection {
    /// Create a new root array collection with zero offset/length.
    pub fn new_array() -> Self {
        CborCollection::Array(
            Value::Array(Vec::new()),
            None,
            0,
            CborPos {
                offset: 0,
                length: 0,
            },
            CborPos {
                offset: 0,
                length: 0,
            },
        )
    }

    /// Create a new collection from the given token (array, map, tag, or indefinite string/bytes).
    pub fn new_collection(token: &ExtendedToken) -> Result<Self, JsError> {
        let pos = CborPos {
            offset: token.offset,
            length: token.length,
        };

        match token.token {
            Token::BeginArray => Ok(CborCollection::Array(
                Value::Array(Vec::new()),
                None,
                0,
                pos.clone(),
                pos,
            )),
            Token::BeginMap => Ok(CborCollection::Map(
                Value::Array(Vec::new()),
                None,
                None,
                0,
                pos.clone(),
                pos,
            )),
            Token::Array(len) => Ok(CborCollection::Array(
                Value::Array(Vec::new()),
                Some(len as usize),
                0,
                pos.clone(),
                pos,
            )),
            Token::Map(len) => Ok(CborCollection::Map(
                Value::Array(Vec::new()),
                None,
                Some(len as usize),
                0,
                pos.clone(),
                pos,
            )),
            Token::Tag(tag) => Ok(CborCollection::Tag(None, tag, pos.clone(), pos)),

            // Indefinite string/bytes => store chunk tokens in a Vec
            Token::BeginString => Ok(CborCollection::IndefiniteString(
                Vec::new(),
                pos.clone(),
                pos,
            )),
            Token::BeginBytes => Ok(CborCollection::IndefiniteBytes(
                Vec::new(),
                pos.clone(),
                pos,
            )),

            _ => Err(JsError::new("Invalid token for new collection")),
        }
    }

    /// Add a `new_value` to this collection. If `finalizer` is `true`, it indicates a CBOR `break`.
    pub fn add_value(
        &mut self,
        new_value: Value,
        value_pos: &CborPos,
        finalizer: bool,
    ) -> Result<(), JsError> {
        match self {
            // ---------------------------------------------------------
            // Arrays
            // ---------------------------------------------------------
            CborCollection::Array(array, len, count, _, total_size) => {
                // Enforce definite-length array if given
                if let Some(max_len) = *len {
                    if *count >= max_len {
                        return Err(JsError::new("Array is already full"));
                    }
                }
                let arr = array
                    .as_array_mut()
                    .ok_or_else(|| JsError::new("Array collection is not actually array"))?;
                arr.push(new_value);

                if !finalizer {
                    *count += 1;
                }
                *total_size = extend_pos(total_size, value_pos);
                Ok(())
            }

            // ---------------------------------------------------------
            // Maps
            // ---------------------------------------------------------
            CborCollection::Map(map_val, key_slot, len, count, _, total_size) => {
                if let Some(max_len) = *len {
                    if *count >= max_len {
                        return Err(JsError::new("Map is already full"));
                    }
                }
                if key_slot.is_none() {
                    // Next value is the key
                    *key_slot = Some(new_value);
                    return Ok(());
                }
                // We have a key, so new_value is the map's value
                let map_array = map_val
                    .as_array_mut()
                    .ok_or_else(|| JsError::new("Map collection is not stored as array"))?;

                let key = key_slot.take().expect("Key was unexpectedly empty");
                map_array.push(build_map_value(key, new_value));

                if !finalizer {
                    *count += 1;
                }
                *total_size = extend_pos(total_size, value_pos);
                Ok(())
            }

            // ---------------------------------------------------------
            // Tag
            // ---------------------------------------------------------
            CborCollection::Tag(ref mut stored_value, _, _, total_size) => {
                if stored_value.is_some() {
                    return Err(JsError::new("Tag already has a value"));
                }
                *stored_value = Some(new_value);
                *total_size = extend_pos(total_size, value_pos);
                Ok(())
            }

            // ---------------------------------------------------------
            // Indefinite string => store each chunk as an element in `Vec<Value>`
            // ---------------------------------------------------------
            CborCollection::IndefiniteString(chunks, _, total_size) => {
                // If finalizer == true (break), we do not add a new chunk
                if !finalizer {
                    chunks.push(new_value);
                }
                *total_size = extend_pos(total_size, value_pos);
                Ok(())
            }

            // ---------------------------------------------------------
            // Indefinite bytes => store each chunk in `Vec<Value>` as well
            // ---------------------------------------------------------
            CborCollection::IndefiniteBytes(chunks, _, total_size) => {
                if !finalizer {
                    chunks.push(new_value);
                }
                *total_size = extend_pos(total_size, value_pos);
                Ok(())
            }
        }
    }

    /// Returns true if this collection is complete on its own (without a break).
    /// - Arrays/Maps are complete if they have definite length and we've reached it.
    /// - Tags are complete once they have a value.
    /// - Indefinite strings/bytes never "auto-complete" (need a break).
    pub fn is_collection_finished(&self) -> bool {
        match self {
            CborCollection::Array(_, Some(len), count, _, _) => *count >= *len,
            CborCollection::Map(_, _, Some(len), count, _, _) => *count >= *len,
            CborCollection::Tag(Some(_), _, _, _) => true,

            // Indefinite strings/bytes do not finish unless a break is encountered externally
            CborCollection::IndefiniteString(_, _, _) => false,
            CborCollection::IndefiniteBytes(_, _, _) => false,

            // Indefinite array/map also never finishes automatically
            _ => false,
        }
    }

    /// Finalize this collection and convert it to a full JSON Value (with metadata).
    pub fn to_value(self) -> Result<Value, JsError> {
        match self {
            // ---------------------------------------------------------
            // Array
            // ---------------------------------------------------------
            CborCollection::Array(array_val, length, _, pos, full_struct_pos) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("Array".into()));
                let len_val = match length {
                    Some(n) => Value::Number(n.into()),
                    None => Value::String("Indefinite".into()),
                };
                obj.insert("items".into(), len_val);
                obj.insert("position_info".into(), cbor_pos_to_value(&pos));
                obj.insert(
                    "struct_position_info".into(),
                    cbor_pos_to_value(&full_struct_pos),
                );
                obj.insert("values".into(), array_val);
                Ok(Value::Object(obj))
            }

            // ---------------------------------------------------------
            // Map
            // ---------------------------------------------------------
            CborCollection::Map(map_val, _, length, _, pos, full_struct_pos) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("Map".into()));
                let len_val = match length {
                    Some(n) => Value::Number(n.into()),
                    None => Value::String("Indefinite".into()),
                };
                obj.insert("items".into(), len_val);
                obj.insert("position_info".into(), cbor_pos_to_value(&pos));
                obj.insert(
                    "struct_position_info".into(),
                    cbor_pos_to_value(&full_struct_pos),
                );
                obj.insert("values".into(), map_val);
                Ok(Value::Object(obj))
            }

            // ---------------------------------------------------------
            // Tag
            // ---------------------------------------------------------
            CborCollection::Tag(value, tag, pos, full_struct_pos) => {
                let val = value.ok_or_else(|| JsError::new("Tag has no value"))?;
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("Tag".into()));
                obj.insert("position_info".into(), cbor_pos_to_value(&pos));
                obj.insert(
                    "struct_position_info".into(),
                    cbor_pos_to_value(&full_struct_pos),
                );
                obj.insert("tag".into(), Value::String(get_tag_name(&tag)));
                obj.insert("value".into(), val);
                Ok(Value::Object(obj))
            }

            // ---------------------------------------------------------
            // Indefinite String => store chunks as an array
            // ---------------------------------------------------------
            CborCollection::IndefiniteString(chunks, pos, full_struct_pos) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("IndefiniteLengthString".into()));
                obj.insert("position_info".into(), cbor_pos_to_value(&pos));
                obj.insert(
                    "struct_position_info".into(),
                    cbor_pos_to_value(&full_struct_pos),
                );
                // Instead of a single concatenated string, we show them as `chunks`
                // which is an array of token objects.
                obj.insert("chunks".into(), Value::Array(chunks));
                Ok(Value::Object(obj))
            }

            // ---------------------------------------------------------
            // Indefinite Bytes => store chunks as an array
            // ---------------------------------------------------------
            CborCollection::IndefiniteBytes(chunks, pos, full_struct_pos) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("IndefiniteLengthBytes".into()));
                obj.insert("position_info".into(), cbor_pos_to_value(&pos));
                obj.insert(
                    "struct_position_info".into(),
                    cbor_pos_to_value(&full_struct_pos),
                );
                // Show chunk tokens as an array
                obj.insert("chunks".into(), Value::Array(chunks));
                Ok(Value::Object(obj))
            }
        }
    }

    /// Convert to a *simple* value, discarding metadata.
    /// For indefinite string/bytes, we just store an array of chunk tokens as JSON.
    pub fn to_simple_value(self) -> Value {
        match self {
            CborCollection::Array(vals, _, _, _, _) => vals,
            CborCollection::Map(vals, _, _, _, _, _) => vals,
            CborCollection::Tag(val, _, _, _) => val.unwrap_or(Value::Null),
            CborCollection::IndefiniteString(chunks, _, _) => Value::Array(chunks),
            CborCollection::IndefiniteBytes(chunks, _, _) => Value::Array(chunks),
        }
    }

    /// Get the overall position info for this collection
    pub fn get_full_pos(&self) -> CborPos {
        match self {
            CborCollection::Array(_, _, _, _, pos) => pos.clone(),
            CborCollection::Map(_, _, _, _, _, pos) => pos.clone(),
            CborCollection::Tag(_, _, _, pos) => pos.clone(),
            CborCollection::IndefiniteString(_, _, pos) => pos.clone(),
            CborCollection::IndefiniteBytes(_, _, pos) => pos.clone(),
        }
    }
}

/// Extend the position if `value_pos` goes beyond current `struct_pos`.
pub fn extend_pos(struct_pos: &CborPos, value_pos: &CborPos) -> CborPos {
    let end_struct = struct_pos.offset + struct_pos.length;
    let end_value = value_pos.offset + value_pos.length;
    if end_value > end_struct {
        CborPos {
            offset: struct_pos.offset,
            length: end_value - struct_pos.offset,
        }
    } else {
        struct_pos.clone()
    }
}

/// Build a map entry object: `{ "key": <...>, "value": <...> }`.
pub fn build_map_value(key: Value, value: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("key".into(), key);
    map.insert("value".into(), value);
    Value::Object(map)
}

/// Convert a `CborPos` into JSON.
pub fn cbor_pos_to_value(pos: &CborPos) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("offset".into(), Value::Number(pos.offset.into()));
    map.insert("length".into(), Value::Number(pos.length.into()));
    Value::Object(map)
}

/// Creates an `ExtendedTokenizer` from raw CBOR data.
pub fn get_tokenizer<'a>(data: &'a[u8]) -> ExtendedTokenizer<'a> {
    Decoder::new(data).into()
}

/// Parse a stream of tokens into a serde_json::Value that includes structure metadata.
pub fn get_value(tokenizer: ExtendedTokenizer) -> Result<Value, JsError> {
    let mut collections = Vec::<CborCollection>::new();
    // Start with a root array as a container
    collections.push(CborCollection::new_array());

    for token_result in tokenizer {
        let token = token_result.map_err(
            |e| JsError::new(&format!("Failed to decode CBOR token: {:?}", e)))?;
        let token_pos = CborPos {
            offset: token.offset,
            length: token.length,
        };

        // Attempt to collapse finished collections
        collections = collapse_collections(collections)?;

        // If we got a `break` => pop the top collection
        if is_break_token(&token.token) {
            let mut last = collections
                .pop()
                .ok_or_else(|| JsError::new("No collection to finalize"))?;
            let break_value = extended_token_to_value(&token, &token_pos)?;
            last.add_value(break_value, &token_pos, true)?;
            let last_pos = last.get_full_pos();

            // Insert the finished collection into its parent
            collections
                .last_mut()
                .ok_or_else(|| JsError::new("No parent collection found"))?
                .add_value(last.to_value()?, &last_pos, false)?;

            continue;
        }

        // Attempt to collapse again
        collections = collapse_collections(collections)?;

        // If this is a new collection start, push it
        if is_token_collection(&token.token) {
            let new_coll = CborCollection::new_collection(&token)?;
            collections.push(new_coll);
            continue;
        }

        // Otherwise, treat it as a simple token => add to current collection
        let new_value = extended_token_to_value(&token, &token_pos)?;
        collections
            .last_mut()
            .ok_or_else(|| JsError::new("No active collection found"))?
            .add_value(new_value, &token_pos, false)?;
    }

    // Final collapse
    collections = collapse_collections(collections)?;

    // We expect exactly one root
    if collections.len() != 1 {
        return Err(JsError::new("Invalid CBOR: unexpected extra collections"));
    }
    Ok(collections.pop().unwrap().to_simple_value())
}

/// Try to collapse the stack of collections if the top is "finished".
pub fn collapse_collections(
    mut stack: Vec<CborCollection>,
) -> Result<Vec<CborCollection>, JsError> {
    while let Some(last) = stack.last() {
        if last.is_collection_finished() {
            let finished = stack.pop().unwrap();
            let pos = finished.get_full_pos();
            let val = finished.to_value()?;

            stack
                .last_mut()
                .ok_or_else(|| JsError::new("No parent collection to collapse into"))?
                .add_value(val, &pos, false)?;
        } else {
            break;
        }
    }
    Ok(stack)
}

/// Check if `token` starts a new collection (map, array, tag, or indefinite string/bytes).
pub fn is_token_collection(token: &Token) -> bool {
    matches!(
        token,
        Token::BeginArray
            | Token::BeginMap
            | Token::Array(_)
            | Token::Map(_)
            | Token::Tag(_)
            | Token::BeginString
            | Token::BeginBytes
    )
}

/// Check if `token` is the break marker for an indefinite-length collection.
pub fn is_break_token(token: &Token) -> bool {
    matches!(token, Token::Break)
}

/// Convert an `ExtendedToken` to JSON: { "position_info", "type", "value" }
pub fn extended_token_to_value(token: &ExtendedToken, pos: &CborPos) -> Result<Value, JsError> {
    let mut map = serde_json::Map::new();
    map.insert("position_info".into(), cbor_pos_to_value(pos));
    map.insert("type".into(), Value::String(get_token_name(&token.token)));
    let val = token_to_value(&token.token)?;
    map.insert("value".into(), val);
    Ok(Value::Object(map))
}

/// Convert a single Token to a JSON value. Collection starters return error.
pub fn token_to_value(token: &Token) -> Result<Value, JsError> {
    match *token {
        Token::Null | Token::Undefined | Token::Break => Ok(Value::Null),
        Token::Bool(b) => Ok(Value::Bool(b)),
        Token::U8(u) => Ok(Value::Number(u.into())),
        Token::U16(u) => Ok(Value::Number(u.into())),
        Token::U32(u) => Ok(Value::Number(u.into())),
        Token::U64(u) => Ok(Value::Number(u.into())),
        Token::I8(i) => Ok(Value::Number(i.into())),
        Token::I16(i) => Ok(Value::Number(i.into())),
        Token::I32(i) => Ok(Value::Number(i.into())),
        Token::I64(i) => Ok(Value::Number(i.into())),
        Token::Int(i) => {
            let u: i128 = i.into();
            Ok(Value::Number(Number::from_i128(u).ok_or_else(|| {
                JsError::new(&format!("Can't covert CBOR int into js Value. {:?}", i))
            })?))
        }
        Token::F16(f) => num_to_f64(f.into()),
        Token::F32(f) => num_to_f64(f.into()),
        Token::F64(f) => num_to_f64(f),
        Token::Bytes(b) => Ok(Value::String(hex::encode(b))),
        Token::String(s) => Ok(Value::String(s.to_string())),
        Token::Simple(simple) => Ok(Value::Number(simple.into())),

        // Collection starters => Not direct values
        Token::BeginArray
        | Token::BeginMap
        | Token::Array(_)
        | Token::Map(_)
        | Token::Tag(_)
        | Token::BeginString
        | Token::BeginBytes => Err(JsError::new("Collection token cannot be a direct value")),
    }
}

/// Convert float to JSON number if possible.
fn num_to_f64(f: f64) -> Result<Value, JsError> {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| JsError::new("Failed to convert float"))
}

/// Return a human-readable name for each Token variant.
pub fn get_token_name(token: &Token) -> String {
    match token {
        Token::Null => "Null",
        Token::Bool(_) => "Bool",
        Token::U8(_) => "U8",
        Token::U16(_) => "U16",
        Token::U32(_) => "U32",
        Token::U64(_) => "U64",
        Token::I8(_) => "I8",
        Token::I16(_) => "I16",
        Token::I32(_) => "I32",
        Token::I64(_) => "I64",
        Token::Int(_) => "Int",
        Token::F16(_) => "F16",
        Token::F32(_) => "F32",
        Token::F64(_) => "F64",
        Token::Bytes(_) => "Bytes",
        Token::String(_) => "String",
        Token::Simple(_) => "Simple",
        Token::Undefined => "Undefined",
        Token::BeginArray => "BeginArray",
        Token::BeginMap => "BeginMap",
        Token::BeginString => "BeginString",
        Token::BeginBytes => "BeginBytes",
        Token::Break => "Break",
        Token::Array(_) => "Array",
        Token::Map(_) => "Map",
        Token::Tag(_) => "Tag",
    }
    .to_string()
}

/// Human-readable tag name
pub fn get_tag_name(tag: &Tag) -> String {
    match tag {
        Tag::DateTime => "DateTime".to_string(),
        Tag::Timestamp => "Timestamp".to_string(),
        Tag::PosBignum => "PosBignum".to_string(),
        Tag::NegBignum => "NegBignum".to_string(),
        Tag::Decimal => "Decimal".to_string(),
        Tag::Bigfloat => "Bigfloat".to_string(),
        Tag::ToBase64Url => "ToBase64Url".to_string(),
        Tag::ToBase64 => "ToBase64".to_string(),
        Tag::ToBase16 => "ToBase16".to_string(),
        Tag::Cbor => "Cbor".to_string(),
        Tag::Uri => "Uri".to_string(),
        Tag::Base64Url => "Base64Url".to_string(),
        Tag::Base64 => "Base64".to_string(),
        Tag::Regex => "Regex".to_string(),
        Tag::Mime => "Mime".to_string(),
        Tag::Unassigned(u) => format!("Unassigned({})", u),
    }
}
