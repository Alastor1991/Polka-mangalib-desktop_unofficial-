//! API отдаёт одни и те же поля то числом, то строкой. Здесь терпимые разборщики.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

pub fn f32_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64().unwrap_or(0.0) as f32,
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    })
}

pub fn u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    })
}

pub fn string_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Число, строка с числом или `null`. Пустая или нечисловая строка – `None`.
pub fn opt_u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Строка, число или `null`.
pub fn opt_string_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// `null` превращается в значение по умолчанию.
pub fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Список, из которого молча выбрасываются элементы, не подходящие под тип.
pub fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let items = Option::<Vec<Value>>::deserialize(d)?.unwrap_or_default();
    Ok(items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

/// Число, строка с числом или `null`. Пустая или нечисловая строка – `None`.
pub fn opt_f32_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n.as_f64().map(|n| n as f32),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Вложенный объект, который не обязан разобраться: если он не подходит под тип,
/// поле остаётся `None`, а не роняет весь объект вокруг. Нужен там, где из-за
/// второстепенного поля иначе пропал бы целый элемент списка.
pub fn opt_lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}
