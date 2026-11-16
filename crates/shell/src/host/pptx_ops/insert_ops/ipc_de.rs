use jscompat::serde::de::{
    self, DeserializeOwned, IntoDeserializer, MapAccess, SeqAccess, Visitor,
};
use shell_contract::IpcValue;

pub(in crate::host::pptx_ops) fn decode<T: DeserializeOwned>(
    value: &IpcValue,
) -> Result<T, String> {
    jscompat::reading_leniently(|| T::deserialize(Value(value))).map_err(|e| e.to_string())
}
struct Value<'a>(&'a IpcValue);
impl<'de> de::Deserializer<'de> for Value<'de> {
    type Error = de::value::Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        match self.0 {
            IpcValue::Undefined | IpcValue::Null => visitor.visit_unit(),
            IpcValue::Bool(v) => visitor.visit_bool(*v),
            IpcValue::Number(v) if !v.is_finite() => visitor.visit_str(if v.is_nan() {
                "NaN"
            } else if v.is_sign_negative() {
                "-Infinity"
            } else {
                "Infinity"
            }),
            IpcValue::Number(v)
                if v.is_finite()
                    && v.fract() == 0.0
                    && v.abs() < 9_007_199_254_740_992.0
                    && !(*v == 0.0 && v.is_sign_negative()) =>
            {
                visitor.visit_i64(*v as i64)
            }
            IpcValue::Number(v) => visitor.visit_f64(*v),
            IpcValue::String(v) => visitor.visit_borrowed_str(v),
            IpcValue::Bytes(_, v) => visitor.visit_seq(ByteSequence(v.iter())),
            IpcValue::Date(v) => visitor.visit_f64(*v),
            IpcValue::Array(v) => visitor.visit_seq(Sequence(v.iter())),
            IpcValue::Object(v) => visitor.visit_map(Object {
                iter: v.iter(),
                value: None,
            }),
        }
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if matches!(self.0, IpcValue::Undefined | IpcValue::Null) {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            IpcValue::String(s) => visitor.visit_enum(s.as_str().into_deserializer()),
            _ => Err(de::Error::custom("expected enum string")),
        }
    }
    jscompat::serde::forward_to_deserialize_any! { bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier ignored_any }
}
struct ByteSequence<'a>(std::slice::Iter<'a, u8>);
impl<'de> SeqAccess<'de> for ByteSequence<'de> {
    type Error = de::value::Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.0
            .next()
            .map(|v| seed.deserialize((*v).into_deserializer()))
            .transpose()
    }
}
struct Sequence<'a>(std::slice::Iter<'a, IpcValue>);
impl<'de> SeqAccess<'de> for Sequence<'de> {
    type Error = de::value::Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.0
            .next()
            .map(|v| seed.deserialize(Value(v)))
            .transpose()
    }
}
struct Object<'a> {
    iter: std::slice::Iter<'a, (String, IpcValue)>,
    value: Option<&'a IpcValue>,
}
impl<'de> MapAccess<'de> for Object<'de> {
    type Error = de::value::Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self
            .iter
            .find(|(_, value)| !matches!(value, IpcValue::Undefined))
        {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(key.as_str().into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        seed.deserialize(Value(
            self.value
                .take()
                .ok_or_else(|| de::Error::custom("missing object value"))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonfinite_numbers_follow_binding_spelling() {
        assert!(decode::<f64>(&IpcValue::Number(f64::INFINITY)).is_err());
        let n: jscompat::JsNum = decode(&IpcValue::Number(f64::INFINITY)).unwrap();
        assert!(n.0.is_infinite());
    }
    #[test]
    fn media_poster_preserves_binary() {
        let value = IpcValue::Object(vec![
            (
                "bytes".into(),
                IpcValue::Bytes(shell_contract::BytesKind::Uint8Array, vec![0, 255]),
            ),
            ("ext".into(), IpcValue::String("png".into())),
        ]);
        let poster: pptx_engine::media_insert::MediaBytes = decode(&value).unwrap();
        assert_eq!(poster.bytes, vec![0, 255]);
        assert_eq!(poster.ext, "png");
    }
    #[test]
    fn paragraph_input_matches_lenient_engine_binding() {
        let input = IpcValue::Object(vec![
            ("runs".into(), IpcValue::Array(vec![])),
            ("level".into(), IpcValue::String("2".into())),
            ("omitted".into(), IpcValue::Undefined),
        ]);
        let actual: pptx_engine::types::Paragraph = decode(&input).unwrap();
        let expected: pptx_engine::types::Paragraph = jscompat::reading_leniently(|| {
            serde_json::from_value(serde_json::json!({"runs": [], "level": "2"}))
        })
        .unwrap();
        assert_eq!(actual, expected);
        let unknown = IpcValue::Object(vec![
            ("runs".into(), IpcValue::Array(vec![])),
            ("extra".into(), IpcValue::Bool(true)),
        ]);
        let engine = jscompat::reading_leniently(|| {
            serde_json::from_value::<pptx_engine::types::Paragraph>(
                serde_json::json!({"runs": [], "extra": true}),
            )
        });
        assert_eq!(
            decode::<pptx_engine::types::Paragraph>(&unknown).is_err(),
            engine.is_err()
        );
    }
}
