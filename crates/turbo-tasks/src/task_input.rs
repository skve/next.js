use std::{
    any::{type_name, Any},
    fmt::{Debug, Display},
    future::Future,
    hash::Hash,
    pin::Pin,
    sync::Arc,
};

use anyhow::{anyhow, Result};
use serde::{ser::SerializeTuple, Deserialize, Serialize};

use crate::{
    backend::SlotContent,
    id::{FunctionId, TraitTypeId},
    magic_any::MagicAny,
    manager::{read_task_output, read_task_slot},
    registry, turbo_tasks,
    util::try_join_all,
    value::{TransientValue, Value},
    value_type::TypedForInput,
    RawVc, TaskId, TraitType, Typed, ValueTypeId,
};

#[derive(Clone)]
pub struct SharedReference(pub Option<ValueTypeId>, pub Arc<dyn Any + Send + Sync>);

impl SharedReference {
    pub fn downcast<T: Any + Send + Sync>(self) -> Option<Arc<T>> {
        match Arc::downcast(self.1) {
            Ok(data) => Some(data),
            Err(_) => None,
        }
    }
}

impl Hash for SharedReference {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Hash::hash(&(&*self.1 as *const (dyn Any + Send + Sync)), state)
    }
}
impl PartialEq for SharedReference {
    fn eq(&self, other: &Self) -> bool {
        PartialEq::eq(
            &(&*self.1 as *const (dyn Any + Send + Sync)),
            &(&*other.1 as *const (dyn Any + Send + Sync)),
        )
    }
}
impl Eq for SharedReference {}
impl PartialOrd for SharedReference {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        PartialOrd::partial_cmp(
            &(&*self.1 as *const (dyn Any + Send + Sync)),
            &(&*other.1 as *const (dyn Any + Send + Sync)),
        )
    }
}
impl Ord for SharedReference {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        Ord::cmp(
            &(&*self.1 as *const (dyn Any + Send + Sync)),
            &(&*other.1 as *const (dyn Any + Send + Sync)),
        )
    }
}
impl Debug for SharedReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedReference")
            .field(&self.0)
            .field(&self.1)
            .finish()
    }
}

impl Serialize for SharedReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let SharedReference(Some(ty), arc) = self {
            let value_type = registry::get_value_type(*ty);
            if let Some(serializable) = value_type.any_as_serializable(arc) {
                let mut t = serializer.serialize_tuple(2)?;
                t.serialize_element(registry::get_value_type_global_name(*ty))?;
                t.serialize_element(serializable)?;
                t.end()
            } else {
                Err(serde::ser::Error::custom(format!(
                    "{:?} is not serializable",
                    arc
                )))
            }
        } else {
            Err(serde::ser::Error::custom(
                "untyped values are not serializable",
            ))
        }
    }
}

impl Display for SharedReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ty) = self.0 {
            write!(f, "value of type {}", registry::get_value_type(ty).name)
        } else {
            write!(f, "untyped value")
        }
    }
}

impl<'de> Deserialize<'de> for SharedReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SharedReference;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a serializable shared reference")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if let Some(global_name) = seq.next_element()? {
                    if let Some(ty) = registry::get_value_type_id_by_global_name(global_name) {
                        if let Some(seed) = registry::get_value_type(ty).get_any_deserialize_seed()
                        {
                            if let Some(value) = seq.next_element_seed(seed)? {
                                Ok(SharedReference(Some(ty), value.into()))
                            } else {
                                Err(serde::de::Error::invalid_length(
                                    1,
                                    &"tuple with type and value",
                                ))
                            }
                        } else {
                            Err(serde::de::Error::custom(format!(
                                "{ty} is not deserializable"
                            )))
                        }
                    } else {
                        Err(serde::de::Error::unknown_variant(global_name, &[]))
                    }
                } else {
                    Err(serde::de::Error::invalid_length(
                        0,
                        &"tuple with type and value",
                    ))
                }
            }
        }

        deserializer.deserialize_tuple(2, Visitor)
    }
}

#[derive(Debug, Clone, PartialOrd, Ord)]
pub struct SharedValue(pub Option<ValueTypeId>, pub Arc<dyn MagicAny>);

impl SharedValue {
    pub fn downcast<T: Any + Send + Sync>(self) -> Option<Arc<T>> {
        match Arc::downcast(self.1.magic_any_arc()) {
            Ok(data) => Some(data),
            Err(_) => None,
        }
    }
}

impl PartialEq for SharedValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && &self.1 == &other.1
    }
}

impl Eq for SharedValue {}

impl Hash for SharedValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.hash(state);
    }
}

impl Display for SharedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ty) = self.0 {
            write!(f, "value of type {}", registry::get_value_type(ty).name)
        } else {
            write!(f, "untyped value")
        }
    }
}

impl Serialize for SharedValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let SharedValue(Some(ty), arc) = self {
            let value_type = registry::get_value_type(*ty);
            if let Some(serializable) = value_type.magic_as_serializable(arc) {
                let mut t = serializer.serialize_tuple(2)?;
                t.serialize_element(registry::get_value_type_global_name(*ty))?;
                t.serialize_element(serializable)?;
                t.end()
            } else {
                Err(serde::ser::Error::custom(format!(
                    "{:?} is not serializable",
                    arc
                )))
            }
        } else {
            Err(serde::ser::Error::custom(
                "untyped values are not serializable",
            ))
        }
    }
}

impl<'de> Deserialize<'de> for SharedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SharedValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a serializable shared value")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if let Some(global_name) = seq.next_element()? {
                    if let Some(ty) = registry::get_value_type_id_by_global_name(global_name) {
                        if let Some(seed) =
                            registry::get_value_type(ty).get_magic_deserialize_seed()
                        {
                            if let Some(value) = seq.next_element_seed(seed)? {
                                Ok(SharedValue(Some(ty), value.into()))
                            } else {
                                Err(serde::de::Error::invalid_length(
                                    1,
                                    &"tuple with type and value",
                                ))
                            }
                        } else {
                            Err(serde::de::Error::custom(format!(
                                "{ty} is not deserializable"
                            )))
                        }
                    } else {
                        Err(serde::de::Error::unknown_variant(global_name, &[]))
                    }
                } else {
                    Err(serde::de::Error::invalid_length(
                        0,
                        &"tuple with type and value",
                    ))
                }
            }
        }

        deserializer.deserialize_tuple(2, Visitor)
    }
}

#[allow(clippy::derive_hash_xor_eq)]
#[derive(Debug, Hash, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskInput {
    TaskOutput(TaskId),
    TaskSlot(TaskId, usize),
    List(Vec<TaskInput>),
    String(String),
    Bool(bool),
    Usize(usize),
    I32(i32),
    U32(u32),
    Nothing,
    SharedValue(SharedValue),
    SharedReference(SharedReference),
}

impl TaskInput {
    pub async fn resolve_to_value(self) -> Result<TaskInput> {
        let tt = turbo_tasks();
        let mut current = self;
        loop {
            current = match current {
                TaskInput::TaskOutput(task_id) => read_task_output(&*tt, task_id).await?.into(),
                TaskInput::TaskSlot(task_id, index) => {
                    read_task_slot(&*tt, task_id, index).await?.into()
                }
                _ => return Ok(current),
            }
        }
    }

    pub async fn resolve(self) -> Result<TaskInput> {
        let tt = turbo_tasks();
        let mut current = self;
        loop {
            current = match current {
                TaskInput::TaskOutput(task_id) => read_task_output(&*tt, task_id).await?.into(),
                TaskInput::List(list) => {
                    if list.iter().all(|i| i.is_resolved()) {
                        return Ok(TaskInput::List(list));
                    }
                    fn resolve_all(
                        list: Vec<TaskInput>,
                    ) -> Pin<Box<dyn Future<Output = Result<Vec<TaskInput>>> + Send>>
                    {
                        Box::pin(try_join_all(list.into_iter().map(|i| i.resolve())))
                    }
                    return Ok(TaskInput::List(resolve_all(list).await?));
                }
                _ => return Ok(current),
            }
        }
    }

    pub fn get_task_id(&self) -> Option<TaskId> {
        match self {
            TaskInput::TaskOutput(t) | TaskInput::TaskSlot(t, _) => Some(*t),
            _ => None,
        }
    }

    pub fn get_trait_method(&self, trait_type: TraitTypeId, name: String) -> Option<FunctionId> {
        match self {
            TaskInput::TaskOutput(_) | TaskInput::TaskSlot(_, _) => {
                panic!("get_trait_method must be called on a resolved TaskInput")
            }
            TaskInput::SharedValue(SharedValue(ty, _))
            | TaskInput::SharedReference(SharedReference(ty, _)) => {
                if let Some(ty) = *ty {
                    registry::get_value_type(ty)
                        .trait_methods
                        .get(&(trait_type, name))
                        .map(|r| *r)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn has_trait(&self, trait_type: TraitTypeId) -> bool {
        match self {
            TaskInput::TaskOutput(_) | TaskInput::TaskSlot(_, _) => {
                panic!("has_trait() must be called on a resolved TaskInput")
            }
            TaskInput::SharedValue(SharedValue(ty, _))
            | TaskInput::SharedReference(SharedReference(ty, _)) => {
                if let Some(ty) = *ty {
                    registry::get_value_type(ty).traits.contains(&trait_type)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn traits(&self) -> Vec<&'static TraitType> {
        match self {
            TaskInput::TaskOutput(_) | TaskInput::TaskSlot(_, _) => {
                panic!("traits() must be called on a resolved TaskInput")
            }
            TaskInput::SharedValue(SharedValue(ty, _))
            | TaskInput::SharedReference(SharedReference(ty, _)) => {
                if let Some(ty) = *ty {
                    registry::get_value_type(ty)
                        .traits
                        .iter()
                        .map(|t| registry::get_trait(*t))
                        .collect()
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    pub fn is_resolved(&self) -> bool {
        match self {
            TaskInput::TaskOutput(_) => false,
            TaskInput::List(list) => list.iter().all(|i| i.is_resolved()),
            _ => true,
        }
    }

    pub fn is_nothing(&self) -> bool {
        match self {
            TaskInput::Nothing => true,
            _ => false,
        }
    }
}

impl From<RawVc> for TaskInput {
    fn from(raw_vc: RawVc) -> Self {
        match raw_vc {
            RawVc::TaskOutput(task) => TaskInput::TaskOutput(task),
            RawVc::TaskSlot(task, i) => TaskInput::TaskSlot(task, i),
        }
    }
}

impl From<SlotContent> for TaskInput {
    fn from(content: SlotContent) -> Self {
        match content {
            SlotContent(None) => TaskInput::Nothing,
            SlotContent(Some(shared_ref)) => TaskInput::SharedReference(shared_ref),
        }
    }
}

impl Display for TaskInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskInput::TaskOutput(task) => write!(f, "task output {}", task),
            TaskInput::TaskSlot(task, index) => write!(f, "slot {} in {}", index, task),
            TaskInput::List(list) => write!(
                f,
                "list {}",
                list.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TaskInput::String(s) => write!(f, "string {:?}", s),
            TaskInput::Bool(b) => write!(f, "bool {:?}", b),
            TaskInput::Usize(v) => write!(f, "usize {}", v),
            TaskInput::I32(v) => write!(f, "i32 {}", v),
            TaskInput::U32(v) => write!(f, "u32 {}", v),
            TaskInput::Nothing => write!(f, "nothing"),
            TaskInput::SharedValue(_) => write!(f, "any value"),
            TaskInput::SharedReference(data) => {
                write!(f, "shared reference with {}", data)
            }
        }
    }
}

impl From<String> for TaskInput {
    fn from(s: String) -> Self {
        TaskInput::String(s)
    }
}

impl From<&str> for TaskInput {
    fn from(s: &str) -> Self {
        TaskInput::String(s.to_string())
    }
}

impl From<bool> for TaskInput {
    fn from(b: bool) -> Self {
        TaskInput::Bool(b)
    }
}

impl From<i32> for TaskInput {
    fn from(v: i32) -> Self {
        TaskInput::I32(v)
    }
}

impl From<u32> for TaskInput {
    fn from(v: u32) -> Self {
        TaskInput::U32(v)
    }
}

impl From<usize> for TaskInput {
    fn from(v: usize) -> Self {
        TaskInput::Usize(v)
    }
}

impl<T: Any + Debug + Clone + Hash + Eq + Ord + Typed + TypedForInput + Send + Sync + 'static>
    From<Value<T>> for TaskInput
where
    T: Serialize,
    for<'de2> T: Deserialize<'de2>,
{
    fn from(v: Value<T>) -> Self {
        let raw_value: T = v.into_value();
        TaskInput::SharedValue(SharedValue(
            Some(T::get_value_type_id()),
            Arc::new(raw_value),
        ))
    }
}

impl<T: Any + Debug + Clone + Hash + Eq + Ord + Send + Sync + 'static> From<TransientValue<T>>
    for TaskInput
where
    T: Serialize,
    for<'de2> T: Deserialize<'de2>,
{
    fn from(v: TransientValue<T>) -> Self {
        let raw_value: T = v.into_value();
        TaskInput::SharedValue(SharedValue(None, Arc::new(raw_value)))
    }
}

impl<T: Into<TaskInput>> From<Vec<T>> for TaskInput {
    fn from(s: Vec<T>) -> Self {
        TaskInput::List(s.into_iter().map(|i| i.into()).collect())
    }
}

impl TryFrom<&TaskInput> for String {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::String(str) => Ok(str.to_string()),
            _ => Err(anyhow!("invalid task input type, expected string")),
        }
    }
}

impl<'a> TryFrom<&'a TaskInput> for &'a str {
    type Error = anyhow::Error;

    fn try_from(value: &'a TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::String(str) => Ok(&str),
            _ => Err(anyhow!("invalid task input type, expected string")),
        }
    }
}

impl TryFrom<&TaskInput> for bool {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::Bool(b) => Ok(*b),
            _ => Err(anyhow!("invalid task input type, expected bool")),
        }
    }
}

impl<'a, T: TryFrom<&'a TaskInput, Error = anyhow::Error>> TryFrom<&'a TaskInput> for Vec<T> {
    type Error = anyhow::Error;

    fn try_from(value: &'a TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::List(list) => Ok(list
                .iter()
                .map(|i| i.try_into())
                .collect::<Result<Vec<_>, _>>()?),
            _ => Err(anyhow!("invalid task input type, expected list")),
        }
    }
}

impl TryFrom<&TaskInput> for u32 {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::U32(value) => Ok(*value),
            _ => Err(anyhow!("invalid task input type, expected u32")),
        }
    }
}

impl TryFrom<&TaskInput> for i32 {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::I32(value) => Ok(*value),
            _ => Err(anyhow!("invalid task input type, expected i32")),
        }
    }
}

impl TryFrom<&TaskInput> for usize {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::Usize(value) => Ok(*value),
            _ => Err(anyhow!("invalid task input type, expected usize")),
        }
    }
}

impl<T: Any + Debug + Clone + Hash + Eq + Ord + Typed + Send + Sync + 'static> TryFrom<&TaskInput>
    for Value<T>
where
    T: Serialize,
    for<'de2> T: Deserialize<'de2>,
{
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::SharedValue(value) => {
                let v = value.1.downcast_ref::<T>().ok_or_else(|| {
                    anyhow!(
                        "invalid task input type, expected {} got {:?}",
                        type_name::<T>(),
                        value.1,
                    )
                })?;
                Ok(Value::new(v.clone()))
            }
            _ => Err(anyhow!(
                "invalid task input type, expected {}",
                type_name::<T>()
            )),
        }
    }
}

impl<T: Any + Debug + Clone + Hash + Eq + Ord + Typed + Send + Sync + 'static> TryFrom<&TaskInput>
    for TransientValue<T>
where
    T: Serialize,
    for<'de2> T: Deserialize<'de2>,
{
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::SharedValue(value) => {
                let v = value.1.downcast_ref::<T>().ok_or_else(|| {
                    anyhow!(
                        "invalid task input type, expected {} got {:?}",
                        type_name::<T>(),
                        value.1,
                    )
                })?;
                Ok(TransientValue::new(v.clone()))
            }
            _ => Err(anyhow!(
                "invalid task input type, expected {}",
                type_name::<T>()
            )),
        }
    }
}

impl TryFrom<&TaskInput> for RawVc {
    type Error = anyhow::Error;

    fn try_from(value: &TaskInput) -> Result<Self, Self::Error> {
        match value {
            TaskInput::TaskOutput(task) => Ok(RawVc::TaskOutput(*task)),
            TaskInput::TaskSlot(task, index) => Ok(RawVc::TaskSlot(*task, *index)),
            _ => Err(anyhow!("invalid task input type, expected slot ref")),
        }
    }
}
