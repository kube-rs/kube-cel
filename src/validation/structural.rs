//! Schema-aware CEL values for a CRD rule's `self` and `oldSelf`, as the
//! apiserver builds them (`k8s.io/apiserver/pkg/cel/common`,
//! `UnstructuredToVal`, v0.36.1).
//!
//! Three kinds of node behave unlike a plain CEL list or map there:
//!
//! - a list with `x-kubernetes-list-type: set` ([`SetList`]): `==` ignores the
//!   order, and `+` is a union that keeps the left list's order;
//! - a list with `x-kubernetes-list-type: map` ([`MapList`]): `==` matches
//!   items by their key fields, and `+` merges: a right item replaces the left
//!   one with its key in place, a new key is appended;
//! - an object with declared `properties` ([`Object`]): a property that is
//!   `null` is absent to field access, `has()`, `.?` and `in`, but still
//!   counted by `size()` and iterated; a key the schema does not declare
//!   (`x-kubernetes-preserve-unknown-fields`) is absent too, counted, and
//!   iterated as `null`; `==` follows the apiserver's `unstructuredMap.Equal`.
//!
//! Each keeps its items in the order the object has them, so indexing,
//! iteration and anything that reads the order see what the apiserver sees.
//! The behaviour is the left operand's, as on the apiserver: `self.s == [..]`
//! is a set comparison. `[..] == self.s` is a positional one there, but
//! `cel`'s own lists and maps are unequal to any of another type until `cel`
//! carries cel-rust#462 (see the README divergence table).
//!
//! Only `self` and `oldSelf` of a CRD rule are built so; a policy's values
//! have no schema.

use std::{any::Any, cell::Cell, collections::BTreeMap, sync::Arc};

use cel::{
    ExecutionError, Value,
    common::{
        traits::{Adder, Container, Indexer, Iterable, Iterator as CelIterator, Sizer, Zeroer},
        types::{CelInt, CelList, CelMap, CelMapKey, CelNull, CelString, Kind, LIST_TYPE, MAP_TYPE, Type},
        value::{CowVal, StaticVal, Val},
    },
};

use crate::validation::{
    compilation::CompiledSchema,
    escaping::{go_escape, go_unescape},
    values::{SchemaFormat, json_to_cel, scalar_to_cel},
};

thread_local! {
    /// Set when an evaluation reaches an operation on one of these values that
    /// kube-cel cannot carry out as the apiserver does: an equality the
    /// apiserver fails where `cel` cannot fail ([`Val::equals`] returns a
    /// `bool`), a Go panic (a value that cannot be a map key), a value `+`
    /// adds that does not bind under the list's items, or a result that hangs
    /// on state the apiserver shares between values (see [`AddLog`]).
    static INEXACT: Cell<bool> = const { Cell::new(false) };
    /// Counts evaluations, so that [`AddLog`] tells one from the next.
    static EVALUATION: Cell<u64> = const { Cell::new(0) };
}

/// Notes that the running evaluation reached such an operation, see
/// [`tracking_inexact`].
fn note_inexact() {
    INEXACT.with(|c| c.set(true));
}

/// Runs `f` (one evaluation) and reports whether it reached an operation
/// [`note_inexact`] notes, so the validation engine can refuse the rule even
/// where CEL absorbed the outcome (`|| true`, `exists`, `!`).
pub(crate) fn tracking_inexact<R>(f: impl FnOnce() -> R) -> (R, bool) {
    EVALUATION.with(|e| e.set(e.get().wrapping_add(1)));
    let outer = INEXACT.with(|c| c.replace(false));
    let result = f();
    let reached = INEXACT.with(|c| c.replace(outer));
    (result, reached)
}

/// Whether `==` with `other` on the right is the apiserver's no-such-overload
/// error for one of these values: `other` is not `null` (cel-go's
/// `types.Equal` compares `null` itself) and not of `kind`. `cel` can only
/// answer `false`, which is the apiserver's `!=` (`Equal(..) != True`) but not
/// its `==`, so the evaluation is noted as well.
fn equality_fails(other: &dyn Val, kind: Kind) -> bool {
    let other_kind = other.get_type().kind();
    let fails = other_kind != kind && other_kind != Kind::NullType;
    if fails {
        note_inexact();
    }
    fails
}

/// The elements of any CEL list, as the apiserver's `traits.Lister`: `None`
/// for a value that is not a list.
fn list_items<'b, 'v>(val: &'b (dyn Val + 'v)) -> Option<Vec<&'b (dyn Val + 'v)>> {
    if val.get_type().kind() != LIST_TYPE.kind() {
        return None;
    }
    let mut items = val.as_iterable()?.iter();
    let mut out = Vec::new();
    while let Some(item) = items.next() {
        out.push(item);
    }
    Some(out)
}

/// The error of `+` with a list on the left and `rhs`, not a list, on the right.
fn not_addable(rhs: &dyn Val) -> ExecutionError {
    ExecutionError::UnsupportedBinaryOperator(
        "add",
        Value::List(Arc::new(Vec::new())),
        Value::try_from(rhs).unwrap_or(Value::Null),
    )
}

/// A value as the apiserver holds a list element before binding it: Go's
/// `interface{}`. Elements decoded from the object are its JSON; `+` adds the
/// `Value()` of each right-hand element, which binds again under the left
/// list's items when read.
#[derive(Clone, Debug)]
enum Go {
    /// Decoded JSON (`map[string]interface{}`, `[]interface{}`, a string,
    /// `int64` or `float64`, a bool, nil).
    Raw(serde_json::Value),
    /// A schema-bound list's elements, its `Value()`.
    List(Arc<Vec<Go>>),
    /// A CEL string's, int's, double's or bool's `Value()`.
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A CEL uint's (`uint64`), `null`'s (`structpb.NullValue`) or
    /// duration's (`time.Duration`, in nanoseconds) `Value()`: map keys of
    /// their own types, equal to no decoded JSON value.
    UInt(u64),
    NullValue,
    Duration(i64),
    /// A CEL map that is none of these values: an `additionalProperties`
    /// map, whose Go value is its decoded `map[string]interface{}`, or a
    /// CEL-built map, whose is not; kube-cel cannot tell which.
    PlainMap,
    /// A CEL list that is none of these values: a plain list field, whose Go
    /// value is its decoded `[]interface{}`, or a CEL-built list, whose is
    /// not; kube-cel cannot tell which.
    PlainList,
    /// Any other Go value (a timestamp, bytes): only whether it can be a map
    /// key matters here.
    Opaque {
        hashable: bool,
    },
}

/// `val.Value()`, as the apiserver's `+` and set and map-list lookups take it.
fn go_value(val: &dyn Val) -> Go {
    if let Some(object) = val.downcast_ref::<Object>() {
        return Go::Raw(serde_json::Value::Object((*object.raw).clone()));
    }
    if let Some(set) = val.downcast_ref::<SetList>() {
        return Go::List(Arc::clone(&set.elements));
    }
    if let Some(list) = val.downcast_ref::<MapList>() {
        return Go::List(Arc::clone(&list.elements));
    }
    if let Some(preserved) = val.downcast_ref::<Preserved>() {
        return Go::Raw((*preserved.0).clone());
    }
    match val.get_type().kind() {
        Kind::String | Kind::Int | Kind::Double | Kind::Boolean => match Value::try_from(val) {
            Ok(Value::String(s)) => Go::Str((*s).clone()),
            Ok(Value::Int(i)) => Go::Int(i),
            Ok(Value::Float(f)) => Go::Float(f),
            Ok(Value::Bool(b)) => Go::Bool(b),
            _ => Go::Opaque { hashable: false },
        },
        Kind::NullType => Go::NullValue,
        Kind::UInt | Kind::Duration => match Value::try_from(val) {
            Ok(Value::UInt(u)) => Go::UInt(u),
            Ok(Value::Duration(d)) => d
                .num_nanoseconds()
                .map_or(Go::Opaque { hashable: true }, Go::Duration),
            _ => Go::Opaque { hashable: true },
        },
        Kind::Timestamp => Go::Opaque { hashable: true },
        Kind::Map => Go::PlainMap,
        Kind::List => Go::PlainList,
        _ => Go::Opaque { hashable: false },
    }
}

/// A Go map key of the kinds JSON and CEL scalars give: equal exactly where
/// Go's `==` on `interface{}` is (`int64(1)` is not `float64(1)`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum GoKey {
    Nil,
    Str(String),
    Int(i64),
    /// A non-NaN `float64`, by its bits, `-0` as `+0`.
    Float(u64),
    Bool(bool),
    UInt(u64),
    NullValue,
    Duration(i64),
}

/// What a [`Go`] value is as a map key.
enum Hashed {
    Key(GoKey),
    /// A NaN: hashable, equal to nothing.
    NaN,
    /// A hashable Go value of a kind kube-cel does not model: equal to no
    /// [`GoKey`], maybe to another such value.
    Opaque,
    /// A map, slice or bytes: a Go panic.
    Unhashable,
}

fn float_key(f: f64) -> Hashed {
    if f.is_nan() {
        Hashed::NaN
    } else {
        Hashed::Key(GoKey::Float(if f == 0.0 { 0f64.to_bits() } else { f.to_bits() }))
    }
}

fn hashed(go: &Go) -> Hashed {
    match go {
        Go::Raw(value) => hashed_json(value),
        Go::Bool(b) => Hashed::Key(GoKey::Bool(*b)),
        Go::Str(s) => Hashed::Key(GoKey::Str(s.clone())),
        Go::Int(i) => Hashed::Key(GoKey::Int(*i)),
        Go::Float(f) => float_key(*f),
        Go::UInt(u) => Hashed::Key(GoKey::UInt(*u)),
        Go::NullValue => Hashed::Key(GoKey::NullValue),
        Go::Duration(d) => Hashed::Key(GoKey::Duration(*d)),
        Go::Opaque { hashable: true } => Hashed::Opaque,
        // A decoded map and a CEL-built one alike.
        Go::List(_) | Go::PlainMap | Go::PlainList | Go::Opaque { hashable: false } => Hashed::Unhashable,
    }
}

fn hashed_json(value: &serde_json::Value) -> Hashed {
    match value {
        serde_json::Value::Null => Hashed::Key(GoKey::Nil),
        serde_json::Value::Bool(b) => Hashed::Key(GoKey::Bool(*b)),
        serde_json::Value::String(s) => Hashed::Key(GoKey::Str(s.clone())),
        // The apiserver decodes a JSON integer that fits as `int64`, any
        // other number as `float64`.
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Hashed::Key(GoKey::Int(i)),
            None => float_key(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Hashed::Unhashable,
    }
}

/// `equality.Semantic.DeepEqual` of two decoded JSON values: an `int64` is not
/// a `float64`.
fn go_deep_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value as J;
    match (a, b) {
        (J::Array(x), J::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| go_deep_equal(x, y)),
        (J::Object(x), J::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| go_deep_equal(v, w)))
        }
        _ => matches!((hashed_json(a), hashed_json(b)), (Hashed::Key(p), Hashed::Key(q)) if p == q),
    }
}

/// A map-list item's key, the apiserver's `toMapKey`: the key fields read
/// from the item's decoded map by their *escaped* names (so a key field
/// whose name escaping changes, `a-b` or `namespace`, reads as nil in every
/// item); one to three as a tuple, more as Go's `%v` of the tuple, where
/// `['a b', 'c', 'd', 'e']` and `['a', 'b c', 'd', 'e']` meet; an item that is
/// not a decoded map as an error value, unique.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MapKey {
    Tuple(Vec<GoKey>),
    Text(String),
    Unique(u64),
}

static NEXT_UNIQUE_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `Err` where Go panics (a map or slice in a key tuple).
fn map_key(item: &Go, keys: &[String]) -> Result<MapKey, ()> {
    // Keyed as a decoded map, or as an error value if CEL built it: noted.
    if matches!(item, Go::PlainMap) {
        note_inexact();
    }
    let Go::Raw(serde_json::Value::Object(obj)) = item else {
        return Ok(MapKey::Unique(
            NEXT_UNIQUE_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
    };
    let parts = keys
        .iter()
        .map(|k| obj.get(k).map_or(Hashed::Key(GoKey::Nil), hashed_json));
    if (1..=3).contains(&keys.len()) {
        return parts
            .map(|part| match part {
                Hashed::Key(k) => Ok(k),
                _ => Err(()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(MapKey::Tuple);
    }
    // `fmt.Sprintf("%v", key)` of `[]interface{}`.
    let mut text = Vec::new();
    for (k, part) in keys.iter().zip(parts) {
        text.push(match part {
            Hashed::Key(GoKey::Nil) => "<nil>".to_owned(),
            Hashed::Key(GoKey::Str(s)) => s,
            Hashed::Key(GoKey::Int(i)) => i.to_string(),
            Hashed::Key(GoKey::Bool(b)) => b.to_string(),
            // Go's `%v` of a float, map or slice: not modelled.
            _ => {
                note_inexact();
                format!("{:?}", obj.get(k))
            }
        });
    }
    Ok(MapKey::Text(format!("[{}]", text.join(" "))))
}

/// The apiserver's `getSet()` of `elements`: their keys, and whether any is
/// [`Hashed::Opaque`]. `Err` where building it panics.
fn key_set(elements: &[Go]) -> Result<(std::collections::HashSet<GoKey>, bool), ()> {
    let mut set = std::collections::HashSet::new();
    let mut opaque = false;
    for element in elements {
        match hashed(element) {
            Hashed::Key(k) => {
                set.insert(k);
            }
            Hashed::NaN => {}
            Hashed::Opaque => opaque = true,
            Hashed::Unhashable => return Err(()),
        }
    }
    Ok((set, opaque))
}

/// The error a Go panic is to the apiserver's evaluation, as a stand-in.
fn go_panic(what: &str) -> ExecutionError {
    ExecutionError::function_error("+", format!("the apiserver's evaluation panics: {what}"))
}

/// Hands out the identities of set-list wrappers, see [`AddLog`].
static NEXT_WRAPPER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_wrapper() -> u64 {
    NEXT_WRAPPER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One set `+` that appended: in which evaluation, on which wrapper, from
/// which position, what.
#[derive(Debug)]
struct Append {
    evaluation: u64,
    /// `None` for a set read through a map or list (see [`SetList::wrapper`]).
    wrapper: Option<u64>,
    start: usize,
    /// The appended elements' keys; `None` for one that equals nothing kube-cel
    /// can tell (a NaN or an opaque Go value).
    keys: Vec<Option<GoKey>>,
    /// Whether one of them is an opaque Go value (a timestamp), which may
    /// equal another kube-cel cannot tell.
    opaque: bool,
}

/// The set `+`s that appended to the elements of one set list decoded from
/// the object and the set lists grown from it, while that value is bound.
///
/// The apiserver's set `+` keeps state between values: it adds the new
/// elements to the receiver's cached set, which a later `==` or `+` on that
/// receiver reads, and appends them to the receiver's slice, whose spare
/// capacity another `+` on the same elements may share. kube-cel computes
/// each `+` and `==` afresh and checks here whether that state could change
/// the outcome: a key a previous `+` on the same wrapper added (each read of
/// a field is a new wrapper, as each `Find` is in Go; the `self` of a set
/// node is one wrapper for every rule there, as the apiserver binds one
/// activation per node), or another value appended at a position a previous
/// `+` of the same evaluation appended at (results do not outlive it). Where
/// it could, the evaluation is noted (fail-closed): how Go's slice grew is
/// not known.
#[derive(Debug, Default)]
struct AddLog(std::sync::Mutex<Vec<Append>>);

impl AddLog {
    fn with<R>(&self, f: impl FnOnce(&mut Vec<Append>) -> R) -> R {
        let mut log = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut log)
    }

    /// Whether a previous `+` on `wrapper` added `key`, so that its cached
    /// set holds a key its elements do not.
    fn added_to(&self, wrapper: Option<u64>, key: &GoKey) -> bool {
        let Some(wrapper) = wrapper else {
            return false;
        };
        self.with(|appends| {
            appends
                .iter()
                .any(|a| a.wrapper == Some(wrapper) && a.keys.iter().any(|k| k.as_ref() == Some(key)))
        })
    }

    /// Whether a previous `+` on `wrapper` added an opaque Go value to its
    /// cached set, which a later opaque lookup may find.
    fn added_opaque(&self, wrapper: Option<u64>) -> bool {
        wrapper.is_some_and(|w| self.with(|appends| appends.iter().any(|a| a.wrapper == Some(w) && a.opaque)))
    }

    /// Records `append`, reporting whether it lands on a position a previous
    /// `+` of its evaluation appended another value at.
    fn record(&self, append: Append) -> bool {
        self.with(|appends| {
            let clash = appends
                .iter()
                .filter(|a| a.evaluation == append.evaluation)
                .any(|a| {
                    (append.start..append.start + append.keys.len())
                        .filter(|p| (a.start..a.start + a.keys.len()).contains(p))
                        .any(|p| {
                            let (mine, theirs) = (&append.keys[p - append.start], &a.keys[p - a.start]);
                            mine.is_none() || mine != theirs
                        })
                });
            appends.push(append);
            clash
        })
    }
}

/// One element of a set or map list, bound under the list's items.
#[derive(Debug)]
enum Item {
    /// Decoded from the object, bound when the list was.
    Bound(Box<dyn Val>),
    /// Added by `+`, bound when first read, as the apiserver binds an element
    /// when it reads it: one the items refuse is an error then, not before
    /// (`(self.s + dyn(['a'])).size()` holds in an integer set).
    Added {
        go: Go,
        shape: Option<Arc<Shape>>,
        bound: std::sync::OnceLock<Option<Box<dyn Val>>>,
    },
}

impl Item {
    fn added(go: Go, shape: Option<Arc<Shape>>) -> Item {
        Item::Added {
            go,
            shape,
            bound: std::sync::OnceLock::new(),
        }
    }

    /// The bound element, `None` where the items refuse it.
    fn get(&self) -> Option<&(dyn Val + 'static)> {
        match self {
            Item::Bound(v) => Some(v.as_ref()),
            Item::Added { go, shape, bound } => bound
                .get_or_init(|| bind_go(go, shape.as_ref(), true).ok().map(element))
                .as_deref(),
        }
    }

    /// The bound element where `cel` can only take a value (iteration):
    /// one the items refuse is the apiserver's error value, which a body may
    /// not read, so the evaluation is noted and `null` stands in.
    fn get_or_note(&self) -> &(dyn Val + 'static) {
        self.get().unwrap_or_else(|| {
            note_inexact();
            &UNDECLARED_KEY
        })
    }

    /// The same element for another list (a `+` result): bound again, lazily.
    fn share(&self) -> Item {
        match self {
            Item::Bound(v) => Item::Bound(v.clone_as_boxed()),
            Item::Added { go, shape, .. } => Item::added(go.clone(), shape.clone()),
        }
    }
}

/// The elements of a set or map list, bound under its items.
#[derive(Debug, Clone)]
struct Items(Arc<(Vec<Item>, CelList<'static>)>);

impl Items {
    fn new(items: Vec<Item>) -> Items {
        // `cel`'s own list of the positions, whose indexer reads an index as
        // `cel` does (an int, a uint, a whole double; out of bounds).
        let positions = (0..items.len())
            .map(|i| Box::new(CelInt::from(i as i64)) as Box<dyn Val>)
            .collect::<Vec<_>>();
        Items(Arc::new((items, CelList::from(positions))))
    }

    fn items(&self) -> &[Item] {
        &self.0.0
    }

    fn shared(&self) -> Vec<Item> {
        self.items().iter().map(Item::share).collect()
    }
}

/// The error of reading an element `+` added that the list's items refuse.
fn refused_element() -> ExecutionError {
    ExecutionError::function_error("_[_]", "invalid data for the list's items (an element + added)")
}

impl Indexer for Items {
    fn get<'b, 'v>(&'b self, idx: &dyn Val) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let positions = self.0.1.as_indexer().expect("a list indexes");
        let position = positions.get(idx)?;
        let i = position
            .downcast_ref::<CelInt>()
            .map(|i| *i.inner() as usize)
            .expect("positions are ints");
        self.items()[i]
            .get()
            .map(CowVal::Borrowed)
            .ok_or_else(refused_element)
    }

    fn steal<'v>(self: Box<Self>, idx: &dyn Val) -> Result<Box<dyn Val + 'v>, ExecutionError>
    where
        Self: 'v,
    {
        self.get(idx).map(CowVal::into_owned)
    }
}

struct ItemsIter<'b>(std::slice::Iter<'b, Item>);

impl<'b, 'v> CelIterator<'b, 'v> for ItemsIter<'b> {
    fn next(&mut self) -> Option<&'b (dyn Val + 'v)> {
        self.0.next().map(Item::get_or_note)
    }
}

impl Iterable for Items {
    fn iter<'b, 'v>(&'b self) -> Box<dyn CelIterator<'b, 'v> + 'b>
    where
        Self: 'v,
    {
        Box::new(ItemsIter(self.items().iter()))
    }
}

impl Sizer for Items {
    fn size(&self) -> CelInt {
        (self.items().len() as i64).into()
    }
}

impl Zeroer for Items {
    fn is_zero_value(&self) -> bool {
        self.items().is_empty()
    }
}

impl Container for Items {
    /// The apiserver's `unstructuredList.Contains`: true where an element
    /// equals `value` (the element on the left), else an error where an
    /// element the items refuse was met, else false.
    fn contains(&self, value: &dyn Val) -> Result<bool, ExecutionError> {
        let mut refused = false;
        for item in self.items() {
            match item.get() {
                Some(element) if element.equals(value) => return Ok(true),
                Some(_) => {}
                None => refused = true,
            }
        }
        if refused {
            Err(refused_element())
        } else {
            Ok(false)
        }
    }
}

/// A list with `x-kubernetes-list-type: set` (`unstructuredSetList`): its
/// elements as the apiserver holds them, which `==` and `+` compare, and
/// bound under its items for everything else.
#[derive(Debug)]
pub(crate) struct SetList {
    elements: Arc<Vec<Go>>,
    items: Items,
    item_shape: Option<Arc<Shape>>,
    log: Arc<AddLog>,
    /// Which apiserver wrapper this is, see [`AddLog`]. A clone is the same
    /// one; a read of the field holding it is a new one. `None` for a set
    /// held in a map or list (an `additionalProperties` map, a list), which
    /// the apiserver binds afresh on each `Find` or `Get`: each use of it is
    /// a new wrapper, and a clone (a variable holding it) a held one.
    wrapper: Option<u64>,
}

impl SetList {
    /// This set as a new wrapper over the same elements, as the apiserver
    /// makes one for each read of a field.
    fn rewrapped(&self) -> SetList {
        SetList {
            wrapper: Some(next_wrapper()),
            ..self.copy()
        }
    }

    /// This set as an element of a map or list.
    fn unanchored(&self) -> SetList {
        SetList {
            wrapper: None,
            ..self.copy()
        }
    }

    fn copy(&self) -> SetList {
        SetList {
            elements: Arc::clone(&self.elements),
            items: self.items.clone(),
            item_shape: self.item_shape.clone(),
            log: Arc::clone(&self.log),
            wrapper: self.wrapper,
        }
    }
}

impl Val for SetList {
    fn get_type(&self) -> &Type {
        <Self as Val>::cel_type()
    }

    fn cel_type() -> &'static Type {
        &LIST_TYPE
    }

    fn as_adder<'b, 'v>(&'b self) -> Option<&'b (dyn Adder + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_container(&self) -> Option<&dyn Container> {
        Some(&self.items)
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(&self.items)
    }

    fn as_iterable<'b, 'v>(&'b self) -> Option<&'b (dyn Iterable + 'v)>
    where
        Self: 'v,
    {
        Some(&self.items)
    }

    fn as_sizer(&self) -> Option<&dyn Sizer> {
        Some(&self.items)
    }

    fn as_zeroer(&self) -> Option<&dyn Zeroer> {
        Some(&self.items)
    }

    /// The apiserver's `unstructuredSetList.Equal`: the sizes match and the
    /// `Value()` of every item of `other` is a key of this set's elements, as
    /// Go holds them. So `self.s == ['a', 'a']` holds for a set `['a', 'b']`,
    /// and a number set decoded from `[1, 2]` (`int64` keys) is unequal to
    /// itself (its items bind as doubles).
    fn equals(&self, other: &dyn Val) -> bool {
        if equality_fails(other, Kind::List) {
            return false;
        }
        let Some(others) = list_items(other) else {
            return false;
        };
        if others.len() != self.elements.len() {
            return false;
        }
        let Ok((set, opaque)) = key_set(&self.elements) else {
            note_inexact();
            return false;
        };
        for item in others {
            match hashed(&go_value(item)) {
                Hashed::Key(k) if set.contains(&k) => {}
                Hashed::Key(k) => {
                    if self.log.added_to(self.wrapper, &k) {
                        note_inexact();
                    }
                    return false;
                }
                Hashed::NaN => return false,
                Hashed::Opaque => {
                    if opaque || self.log.added_opaque(self.wrapper) {
                        note_inexact();
                    }
                    return false;
                }
                Hashed::Unhashable => {
                    note_inexact();
                    return false;
                }
            }
        }
        true
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(SetList {
            wrapper: Some(self.wrapper.unwrap_or_else(next_wrapper)),
            ..self.copy()
        })
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl StaticVal for SetList {}

/// `val` as an element of a map or list: a set there is a new wrapper on
/// each use (see [`SetList::wrapper`]).
fn element(val: Box<dyn Val>) -> Box<dyn Val> {
    match val.downcast_ref::<SetList>() {
        Some(set) => Box::new(set.unanchored()),
        None => val,
    }
}

impl Adder for SetList {
    /// The apiserver's `unstructuredSetList.Add`: this set's elements, then
    /// the `Value()` of each item of `rhs` not among them yet, in `rhs`'s
    /// order, the new ones bound under this set's items. The result is a set.
    fn add<'b, 'v>(&'b self, rhs: &(dyn Val + 'v)) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let others = list_items(rhs).ok_or_else(|| not_addable(rhs))?;
        let start = self.elements.len();
        let (mut set, mut opaque) = key_set(&self.elements).map_err(|()| {
            note_inexact();
            go_panic("a set element cannot be a map key")
        })?;
        let mut elements = (*self.elements).clone();
        let mut items = self.items.shared();
        let mut appended = Vec::new();
        let mut appended_opaque = false;
        for item in others {
            let go = go_value(item);
            let new = match hashed(&go) {
                Hashed::Key(k) => {
                    let new = !set.contains(&k);
                    if new && self.log.added_to(self.wrapper, &k) {
                        note_inexact();
                    }
                    if new {
                        appended.push(Some(k.clone()));
                        set.insert(k);
                    }
                    new
                }
                Hashed::NaN => {
                    appended.push(None);
                    true
                }
                Hashed::Opaque => {
                    if opaque || self.log.added_opaque(self.wrapper) {
                        note_inexact();
                    }
                    opaque = true;
                    appended_opaque = true;
                    appended.push(None);
                    true
                }
                Hashed::Unhashable => {
                    note_inexact();
                    return Err(go_panic("an added element cannot be a map key"));
                }
            };
            if new {
                items.push(Item::added(go.clone(), self.item_shape.clone()));
                elements.push(go);
            }
        }
        if !appended.is_empty()
            && self.log.record(Append {
                evaluation: EVALUATION.with(Cell::get),
                wrapper: self.wrapper,
                start,
                keys: appended,
                opaque: appended_opaque,
            })
        {
            note_inexact();
        }
        Ok(CowVal::owned(SetList {
            elements: Arc::new(elements),
            items: Items::new(items),
            item_shape: self.item_shape.clone(),
            log: Arc::clone(&self.log),
            wrapper: Some(next_wrapper()),
        }))
    }
}

/// A list with `x-kubernetes-list-type: map` (`unstructuredMapList`).
#[derive(Debug)]
pub(crate) struct MapList {
    elements: Arc<Vec<Go>>,
    items: Items,
    item_shape: Option<Arc<Shape>>,
    /// The fields of `x-kubernetes-list-map-keys`, escaped as the apiserver's
    /// `escapeKeyProps` has them (see [`map_key`]).
    keys: Arc<[String]>,
}

impl MapList {
    /// The apiserver's `getMap()`: each element's index by its key, a later
    /// one replacing an earlier. `Err` where Go panics.
    fn key_index(&self) -> Result<std::collections::HashMap<MapKey, usize>, ()> {
        let mut index = std::collections::HashMap::new();
        for (i, element) in self.elements.iter().enumerate() {
            index.insert(map_key(element, &self.keys)?, i);
        }
        Ok(index)
    }
}

impl Val for MapList {
    fn get_type(&self) -> &Type {
        <Self as Val>::cel_type()
    }

    fn cel_type() -> &'static Type {
        &LIST_TYPE
    }

    fn as_adder<'b, 'v>(&'b self) -> Option<&'b (dyn Adder + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_container(&self) -> Option<&dyn Container> {
        Some(&self.items)
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(&self.items)
    }

    fn as_iterable<'b, 'v>(&'b self) -> Option<&'b (dyn Iterable + 'v)>
    where
        Self: 'v,
    {
        Some(&self.items)
    }

    fn as_sizer(&self) -> Option<&dyn Sizer> {
        Some(&self.items)
    }

    fn as_zeroer(&self) -> Option<&dyn Zeroer> {
        Some(&self.items)
    }

    /// The apiserver's `unstructuredMapList.Equal`: the sizes match, and the
    /// `Value()` of each item of `other` has a key ([`map_key`]) of an item
    /// here, which equals it.
    fn equals(&self, other: &dyn Val) -> bool {
        if equality_fails(other, Kind::List) {
            return false;
        }
        let Some(others) = list_items(other) else {
            return false;
        };
        if others.len() != self.elements.len() {
            return false;
        }
        let Ok(mine) = self.key_index() else {
            note_inexact();
            return false;
        };
        for theirs in others {
            let Ok(key) = map_key(&go_value(theirs), &self.keys) else {
                note_inexact();
                return false;
            };
            match mine.get(&key) {
                Some(&i) => match self.items.items()[i].get() {
                    Some(mine) if mine.equals(theirs) => {}
                    Some(_) => return false,
                    // An element the items refuse: the apiserver's `Equal`
                    // is on an error value there.
                    None => {
                        note_inexact();
                        return false;
                    }
                },
                _ => return false,
            }
        }
        true
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(MapList {
            elements: Arc::clone(&self.elements),
            items: self.items.clone(),
            item_shape: self.item_shape.clone(),
            keys: Arc::clone(&self.keys),
        })
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl StaticVal for MapList {}

impl Adder for MapList {
    /// The apiserver's `unstructuredMapList.Add`: this list, with the
    /// `Value()` of an item of `rhs` replacing the one here with its key in
    /// place, and the others appended in `rhs`'s order, each bound under this
    /// list's items. The result is a map list.
    fn add<'b, 'v>(&'b self, rhs: &(dyn Val + 'v)) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let others = list_items(rhs).ok_or_else(|| not_addable(rhs))?;
        let panic = || {
            note_inexact();
            go_panic("a map-list key cannot be a map key")
        };
        let mut elements = (*self.elements).clone();
        let mut merged = self.items.shared();
        // Only this list's keys: an appended item's key is not added, so two
        // new items with one key are both appended.
        let index = self.key_index().map_err(|()| panic())?;
        for item in others {
            let go = go_value(item);
            let key = map_key(&go, &self.keys).map_err(|()| panic())?;
            let bound = Item::added(go.clone(), self.item_shape.clone());
            match index.get(&key).copied() {
                Some(at) => {
                    merged[at] = bound;
                    elements[at] = go;
                }
                None => {
                    merged.push(bound);
                    elements.push(go);
                }
            }
        }
        Ok(CowVal::owned(MapList {
            elements: Arc::new(elements),
            items: Items::new(merged),
            item_shape: self.item_shape.clone(),
            keys: Arc::clone(&self.keys),
        }))
    }
}

/// An object with declared `properties` (`unstructuredMap` with a properties
/// schema).
/// A value under a schema node with no `type` but
/// `x-kubernetes-preserve-unknown-fields` (the apiserver's `unknownPreserved`):
/// `==` with it on the left compares its decoded JSON with the other side's
/// Go value (`Semantic.DeepEqual`); it holds no field or element, so `has()`
/// is false, `.?` none and a plain access an error; `cel` finds no other
/// operation on it, an error as on the apiserver.
#[derive(Debug)]
pub(crate) struct Preserved(Arc<serde_json::Value>);

/// Whether the decoded JSON `raw` and the Go value `go` are deeply equal.
fn deep_equal_go(raw: &serde_json::Value, go: &Go) -> bool {
    match go {
        Go::Raw(value) => go_deep_equal(raw, value),
        Go::List(elements) => raw.as_array().is_some_and(|r| {
            r.len() == elements.len() && r.iter().zip(elements.iter()).all(|(r, g)| deep_equal_go(r, g))
        }),
        Go::Str(_) | Go::Int(_) | Go::Float(_) | Go::Bool(_) => {
            matches!((hashed_json(raw), hashed(go)), (Hashed::Key(p), Hashed::Key(q)) if p == q)
        }
        // A decoded map or list, or a CEL-built one, which is never equal:
        // noted.
        Go::PlainMap | Go::PlainList => {
            note_inexact();
            false
        }
        Go::UInt(_) | Go::NullValue | Go::Duration(_) | Go::Opaque { .. } => false,
    }
}

impl Val for Preserved {
    /// The type `type()` reads, which the apiserver refuses to give such a
    /// value: noted, as `cel` cannot fail there.
    fn get_type(&self) -> &Type {
        note_inexact();
        <Self as Val>::cel_type()
    }

    fn cel_type() -> &'static Type {
        static UNKNOWN: Type = cel::common::types::UNKNOWN_TYPE;
        &UNKNOWN
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn equals(&self, other: &dyn Val) -> bool {
        deep_equal_go(&self.0, &go_value(other))
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(Preserved(Arc::clone(&self.0)))
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl StaticVal for Preserved {}

impl Indexer for Preserved {
    fn get<'b, 'v>(&'b self, idx: &dyn Val) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let key = idx
            .downcast_ref::<CelString>()
            .map_or_else(|| format!("{idx:?}"), |s| s.inner().to_owned());
        Err(ExecutionError::NoSuchKey(Arc::new(key)))
    }

    fn steal<'v>(self: Box<Self>, idx: &dyn Val) -> Result<Box<dyn Val + 'v>, ExecutionError>
    where
        Self: 'v,
    {
        self.get(idx).map(CowVal::into_owned)
    }
}

/// A declared property present in an [`Object`].
#[derive(Debug)]
enum Field {
    /// `null`: absent to CEL.
    Null,
    Bound(Box<dyn Val>),
    /// A value that came with an element `+` added and that the property's
    /// schema refuses: the apiserver binds a field when it reads it, so this
    /// is an error where it is read, not before.
    Refused,
}

impl Field {
    fn copy(&self) -> Field {
        match self {
            Field::Null => Field::Null,
            Field::Bound(v) => Field::Bound(v.clone_as_boxed()),
            Field::Refused => Field::Refused,
        }
    }
}

/// What the apiserver's `Find` finds.
enum Found<'a, 'v> {
    Missing,
    Value(&'a (dyn Val + 'v)),
    /// An error value (see [`Field::Refused`]), `found` all the same.
    Refused,
}

/// One declared field in the apiserver's `unstructuredMap.Equal`: found on
/// one side only is unequal, on neither equal; an error value on either side
/// is an error there (noted).
fn found_equal(mine: Found<'_, '_>, theirs: Found<'_, '_>) -> bool {
    match (mine, theirs) {
        (Found::Value(a), Found::Value(b)) => a.equals(b),
        (Found::Missing, Found::Missing) => true,
        (Found::Missing, _) | (_, Found::Missing) => false,
        (Found::Refused, _) | (_, Found::Refused) => {
            note_inexact();
            false
        }
    }
}

#[derive(Debug)]
pub(crate) struct Object {
    /// The object as decoded, its `Value()`.
    raw: Arc<serde_json::Map<String, serde_json::Value>>,
    /// Whether the node declares properties (Go's `isObject`): a CEL name is
    /// then unescaped to find its field, and a key iterates escaped.
    declares: bool,
    /// The declared properties present, by their JSON name.
    fields: BTreeMap<String, Field>,
    /// The names of `fields`, in order, for iteration.
    names: CelList<'static>,
    /// Keys the schema does not declare, as JSON: hidden from CEL, but
    /// counted by `size()` and compared by `==` (preserve-unknown-fields).
    undeclared: BTreeMap<String, serde_json::Value>,
}

impl Object {
    fn new(
        raw: Arc<serde_json::Map<String, serde_json::Value>>,
        declares: bool,
        fields: BTreeMap<String, Field>,
        undeclared: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        let names = fields
            .keys()
            .map(|k| Box::new(CelString::from(go_escape(k).unwrap_or_else(|| k.clone()))) as Box<dyn Val>)
            .collect::<Vec<_>>();
        Object {
            raw,
            declares,
            fields,
            names: CelList::from(names),
            undeclared,
        }
    }

    /// The apiserver's `Find`: the present, non-null declared property a CEL
    /// name names, unescaped where the node declares properties.
    fn find(&self, name: &str) -> Found<'_, 'static> {
        let field = if self.declares {
            match go_unescape(name) {
                Some(field) => field,
                None => return Found::Missing,
            }
        } else {
            name.to_owned()
        };
        match self.fields.get(&field) {
            None | Some(Field::Null) => Found::Missing,
            Some(Field::Bound(v)) => Found::Value(v.as_ref()),
            Some(Field::Refused) => Found::Refused,
        }
    }

    fn raw_size(&self) -> usize {
        self.fields.len() + self.undeclared.len()
    }
}

impl Val for Object {
    fn get_type(&self) -> &Type {
        <Self as Val>::cel_type()
    }

    fn cel_type() -> &'static Type {
        &MAP_TYPE
    }

    fn as_container(&self) -> Option<&dyn Container> {
        Some(self)
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_iterable<'b, 'v>(&'b self) -> Option<&'b (dyn Iterable + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_sizer(&self) -> Option<&dyn Sizer> {
        Some(self)
    }

    fn as_zeroer(&self) -> Option<&dyn Zeroer> {
        Some(self)
    }

    /// The apiserver's `unstructuredMap.Equal`: the raw sizes (`null`
    /// properties and undeclared keys included) match, each declared property
    /// is present on both sides or neither, comparing equal where present,
    /// and each undeclared key here equals the other's where it has one.
    ///
    /// A value that is not a map, or a map that is not one of these objects
    /// when this one has undeclared keys, is the apiserver's error (noted, see
    /// [`equality_fails`]).
    fn equals(&self, other: &dyn Val) -> bool {
        if equality_fails(other, Kind::Map) {
            return false;
        }
        if let Some(other) = other.downcast_ref::<Object>() {
            // Each raw key: a declared one through `Find` on both sides, which
            // takes it for a CEL name and unescapes it (so `__foo__`, which
            // does not unescape, is absent on both); an undeclared one raw,
            // where the other has it.
            return self.raw_size() == other.raw_size()
                && self.raw.iter().all(|(key, value)| {
                    if self.fields.contains_key(key) {
                        found_equal(self.find(key), other.find(key))
                    } else {
                        other.raw.get(key).is_none_or(|o| go_deep_equal(value, o))
                    }
                });
        }
        // A plain map: what the type checker leaves (`dyn`).
        let Some(size) = other.as_sizer().map(|s| *s.size().inner()) else {
            return false;
        };
        let Some(indexer) = other.as_indexer() else {
            return false;
        };
        if usize::try_from(size).ok() != Some(self.raw_size()) {
            return false;
        }
        // An undeclared key compares raw, which takes the other side's raw
        // map (`*unstructuredMap`); against any other map it is an error. Go
        // visits the keys in random order, so a declared mismatch may answer
        // false first: the apiserver's outcome varies, and is noted either way.
        if !self.undeclared.is_empty() {
            note_inexact();
            return false;
        }
        self.fields.keys().all(|name| {
            // The other map's `Find` takes the raw name as it is.
            let theirs = indexer.get(&CelString::from(name.as_str())).ok();
            found_equal(
                self.find(name),
                theirs
                    .as_ref()
                    .map_or(Found::Missing, |b| Found::Value(b.as_ref())),
            )
        })
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(Object {
            raw: Arc::clone(&self.raw),
            declares: self.declares,
            fields: self.fields.iter().map(|(k, v)| (k.clone(), v.copy())).collect(),
            names: self.names.clone(),
            undeclared: self.undeclared.clone(),
        })
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl StaticVal for Object {}

impl Indexer for Object {
    /// A present, non-null declared property; anything else is absent
    /// (`has()` false, `.?` none), as the apiserver's `Find` has it.
    fn get<'b, 'v>(&'b self, idx: &dyn Val) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let name = idx
            .downcast_ref::<CelString>()
            .ok_or_else(|| ExecutionError::UnexpectedType {
                got: idx.get_type().name().to_owned(),
                want: "string".to_owned(),
            })?;
        let found = match self.find(name.inner()) {
            Found::Value(found) => found,
            Found::Missing => return Err(ExecutionError::NoSuchKey(Arc::new(name.inner().to_owned()))),
            Found::Refused => return Err(refused_element()),
        };
        // Each read of a field is a new wrapper on the apiserver (`Find`
        // binds the value again), which a set list's `+` state tells apart.
        Ok(match found.downcast_ref::<SetList>() {
            Some(set) => CowVal::owned(set.rewrapped()),
            None => CowVal::Borrowed(found),
        })
    }

    fn steal<'v>(self: Box<Self>, idx: &dyn Val) -> Result<Box<dyn Val + 'v>, ExecutionError>
    where
        Self: 'v,
    {
        self.get(idx).map(CowVal::into_owned)
    }
}

/// What an undeclared key iterates as: the apiserver's `Iterator` leaves a nil
/// in its place, which cel-go binds to a comprehension variable as `null`.
static UNDECLARED_KEY: CelNull = CelNull;

/// The keys of an [`Object`], as the apiserver's `unstructuredMap.Iterator`
/// gives them: each declared property present (a `null` one included), then
/// one `null` per undeclared key, so iteration counts the raw keys.
struct ObjectKeys<'b> {
    names: std::slice::Iter<'b, Box<dyn Val>>,
    undeclared: usize,
}

impl<'b, 'v> CelIterator<'b, 'v> for ObjectKeys<'b> {
    fn next(&mut self) -> Option<&'b (dyn Val + 'v)> {
        if let Some(name) = self.names.next() {
            return Some(name.as_ref());
        }
        self.undeclared = self.undeclared.checked_sub(1)?;
        Some(&UNDECLARED_KEY)
    }
}

impl Iterable for Object {
    fn iter<'b, 'v>(&'b self) -> Box<dyn CelIterator<'b, 'v> + 'b>
    where
        Self: 'v,
    {
        Box::new(ObjectKeys {
            names: self.names.inner().iter(),
            undeclared: self.undeclared.len(),
        })
    }
}

impl Container for Object {
    /// The apiserver's `Contains`: whether `Find` finds a field, a key that
    /// is not a string being a no-such-overload error (an undeclared key
    /// iterates as `null`, so `k in dyn(self.o)` reaches one).
    fn contains(&self, value: &dyn Val) -> Result<bool, ExecutionError> {
        let name = value
            .downcast_ref::<CelString>()
            .ok_or_else(|| ExecutionError::UnexpectedType {
                got: value.get_type().name().to_owned(),
                want: "string".to_owned(),
            })?;
        match self.find(name.inner()) {
            Found::Value(_) => Ok(true),
            Found::Missing => Ok(false),
            Found::Refused => Err(refused_element()),
        }
    }
}

impl Sizer for Object {
    fn size(&self) -> CelInt {
        (self.raw_size() as i64).into()
    }
}

impl Zeroer for Object {
    fn is_zero_value(&self) -> bool {
        self.raw_size() == 0
    }
}

/// A converted scalar, or JSON without a schema, as the value converters
/// build it.
fn from_value(value: Value) -> Box<dyn Val> {
    let converted: Result<Box<dyn Val>, _> = value.try_into();
    converted.unwrap_or_else(|_| Box::new(CelNull))
}

/// The list type a schema node declares.
#[derive(Clone, Debug, Default)]
pub(crate) enum ListType {
    /// None, or `atomic`.
    #[default]
    Plain,
    /// `x-kubernetes-list-type: set`.
    Set,
    /// `x-kubernetes-list-type: map`, with its `x-kubernetes-list-map-keys`.
    Map(Arc<[String]>),
}

/// The parts of a structural schema node the apiserver's `UnstructuredToVal`
/// reads to bind a value under it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shape {
    /// `type`.
    ty: Option<String>,
    /// How a scalar binds (`format`, `type: number`, int-or-string).
    format: SchemaFormat,
    /// `nullable`.
    nullable: bool,
    /// `x-kubernetes-embedded-resource`.
    embedded: bool,
    /// `x-kubernetes-preserve-unknown-fields`.
    preserve_unknown: bool,
    /// `properties`; empty ones are none, as in the apiserver's structural
    /// schema.
    properties: Option<BTreeMap<String, Arc<Shape>>>,
    /// `additionalProperties`, when it is a schema.
    additional: Option<Arc<Shape>>,
    /// `items`, when it is a schema.
    items: Option<Arc<Shape>>,
    list: ListType,
}

impl Shape {
    /// The shape of the schema node `schema`.
    pub(crate) fn from_json(schema: &serde_json::Value) -> Arc<Shape> {
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|p| p.iter().map(|(k, s)| (k.clone(), Shape::from_json(s))).collect());
        let items = schema
            .get("items")
            .filter(|i| i.is_object())
            .map(Shape::from_json);
        let additional = schema
            .get("additionalProperties")
            .filter(|a| a.is_object())
            .map(Shape::from_json);
        Shape::node(schema, properties, items, additional)
    }

    /// The shape of `schema`, given its children's.
    pub(crate) fn node(
        schema: &serde_json::Value,
        properties: Option<BTreeMap<String, Arc<Shape>>>,
        items: Option<Arc<Shape>>,
        additional: Option<Arc<Shape>>,
    ) -> Arc<Shape> {
        let list = match schema.get("x-kubernetes-list-type").and_then(|t| t.as_str()) {
            Some("set") => ListType::Set,
            Some("map") => ListType::Map(
                schema
                    .get("x-kubernetes-list-map-keys")
                    .and_then(|k| k.as_array())
                    .map(|k| k.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
            ),
            _ => ListType::Plain,
        };
        Arc::new(Shape {
            ty: schema.get("type").and_then(|t| t.as_str()).map(String::from),
            format: SchemaFormat::from_schema(schema),
            nullable: schema.get("nullable").and_then(serde_json::Value::as_bool) == Some(true),
            embedded: schema
                .get("x-kubernetes-embedded-resource")
                .and_then(serde_json::Value::as_bool)
                == Some(true),
            preserve_unknown: schema
                .get("x-kubernetes-preserve-unknown-fields")
                .and_then(serde_json::Value::as_bool)
                == Some(true),
            properties: properties.filter(|p| !p.is_empty()),
            additional,
            items,
            list,
        })
    }

    fn typed(ty: &str, properties: Option<BTreeMap<String, Arc<Shape>>>) -> Arc<Shape> {
        Arc::new(Shape {
            ty: Some(ty.to_owned()),
            properties,
            ..Shape::default()
        })
    }

    /// The apiserver's `WithTypeAndObjectMeta`, for the resource root and an
    /// embedded resource: `kind` and `apiVersion` declared as strings and
    /// `metadata` as an object of `name` and `generateName` only, unless the
    /// node declares all of them so already.
    fn with_type_and_object_meta(self: &Arc<Self>) -> Arc<Shape> {
        let is = |s: Option<&Arc<Shape>>, ty: &str| s.is_some_and(|s| s.ty.as_deref() == Some(ty));
        if let Some(props) = &self.properties
            && is(props.get("kind"), "string")
            && is(props.get("apiVersion"), "string")
            && let Some(meta) = props
                .get("metadata")
                .filter(|m| m.ty.as_deref() == Some("object"))
            && let Some(meta_props) = &meta.properties
            && is(meta_props.get("name"), "string")
            && is(meta_props.get("generateName"), "string")
        {
            return Arc::clone(self);
        }
        let string = Shape::typed("string", None);
        let mut props = self.properties.clone().unwrap_or_default();
        props.insert("kind".to_owned(), Arc::clone(&string));
        props.insert("apiVersion".to_owned(), Arc::clone(&string));
        props.insert(
            "metadata".to_owned(),
            Shape::typed(
                "object",
                Some(BTreeMap::from([
                    ("name".to_owned(), Arc::clone(&string)),
                    ("generateName".to_owned(), string),
                ])),
            ),
        );
        Arc::new(Shape {
            properties: Some(props),
            ..Shape::clone(self)
        })
    }
}

/// The value an `optionalOldSelf` rule's `oldSelf.value()` holds, given the
/// node's bound `old` and its JSON. cel-go resolves an optional activation
/// variable by taking the bound value's `Value()` and binding it again with
/// `NativeToValue` (`applyQualifiers`): a list or map of the object is then
/// plain JSON (a set a plain list, an object's `null` and undeclared fields
/// present, no field-name escaping), and so is a typeless preserve-unknown
/// value; a scalar's `Value()` binds back as it was.
pub(crate) fn optional_value<'v>(
    old: &(dyn Val + 'v),
    old_raw: Option<&serde_json::Value>,
) -> Box<dyn Val + 'v> {
    let raw = match old_raw {
        Some(raw @ (serde_json::Value::Array(_) | serde_json::Value::Object(_))) => raw,
        Some(raw) if old.downcast_ref::<Preserved>().is_some() => raw,
        _ => return old.clone_as_boxed(),
    };
    from_value(crate::validation::values::json_to_cel_raw(raw))
}

/// Where a node sits: the resource root is bound with `apiVersion`, `kind`
/// and `metadata` declared (the apiserver's `WithTypeAndObjectMeta`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Node {
    /// The resource root.
    Root,
    /// Any other node.
    Inner,
}

/// `value` under the schema node `schema`, as the apiserver binds it.
pub(crate) fn json_to_val_with_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    node: Node,
) -> Box<dyn Val> {
    bind(value, &Shape::from_json(schema), node)
}

/// `value` under the compiled schema node `compiled`, as
/// [`json_to_val_with_schema`] binds it.
pub(crate) fn json_to_val_with_compiled(
    value: &serde_json::Value,
    compiled: &CompiledSchema,
    node: Node,
) -> Box<dyn Val> {
    bind(value, &compiled.shape, node)
}

fn bind(value: &serde_json::Value, shape: &Arc<Shape>, node: Node) -> Box<dyn Val> {
    match node {
        Node::Root => raw_to_val(value, Some(&shape.with_type_and_object_meta())),
        Node::Inner => raw_to_val(value, Some(shape)),
    }
}

/// `value`, decoded from the object, under `shape` (`UnstructuredToVal`);
/// with no shape, a node without `items`, as plain JSON. The JSON's own kind
/// decides, as kube-cel leaves a value the schema's `type` refuses to OpenAPI
/// validation.
fn raw_to_val(value: &serde_json::Value, shape: Option<&Arc<Shape>>) -> Box<dyn Val> {
    bind_raw(value, shape, false).unwrap_or_else(|_| Box::new(CelNull))
}

/// `go` under `shape`. `strict` applies `UnstructuredToVal`'s type checks,
/// which kube-cel leaves to OpenAPI validation for the object's own data but
/// not for a value `+` adds from elsewhere.
fn bind_go(go: &Go, shape: Option<&Arc<Shape>>, strict: bool) -> Result<Box<dyn Val>, String> {
    let refused = || format!("invalid data for the schema: {go:?}");
    match go {
        Go::Raw(value) => bind_raw(value, shape, strict),
        Go::List(elements) => match shape {
            Some(shape) if shape.ty.as_deref() == Some("array") && shape.items.is_some() => {
                bind_list((**elements).clone(), shape, strict)
            }
            _ => Err(refused()),
        },
        Go::Str(_) | Go::Int(_) | Go::Float(_) | Go::Bool(_) => {
            let Some(shape) = shape else {
                return Err(refused());
            };
            let value = match (shape.format == SchemaFormat::IntOrString, shape.ty.as_deref(), go) {
                (true, _, Go::Str(s)) => Value::String(Arc::new(s.clone())),
                (true, _, Go::Int(i)) => Value::Int(*i),
                (true, _, _) => return Err(refused()),
                (false, Some("string"), Go::Str(s)) => strict_string(s, &shape.format).ok_or_else(refused)?,
                (false, Some("number"), Go::Int(i)) => Value::Float(*i as f64),
                (false, Some("number"), Go::Float(f)) => Value::Float(*f),
                (false, Some("integer"), Go::Int(i)) => Value::Int(*i),
                (false, Some("boolean"), Go::Bool(b)) => Value::Bool(*b),
                _ => return Err(refused()),
            };
            Ok(from_value(value))
        }
        Go::UInt(_) | Go::NullValue | Go::Duration(_) | Go::PlainMap | Go::PlainList | Go::Opaque { .. } => {
            Err(refused())
        }
    }
}

/// A string under a `format`, as the apiserver parses it, `None` where it
/// fails.
fn strict_string(s: &str, format: &SchemaFormat) -> Option<Value> {
    use base64::Engine;
    Some(match format {
        SchemaFormat::DateTime => Value::Timestamp(crate::functions::parse_date_time(s).ok()?),
        SchemaFormat::Date => Value::Timestamp(crate::validation::values::parse_date(s)?),
        SchemaFormat::Duration => Value::Duration(crate::validation::values::parse_schema_duration(s)?),
        SchemaFormat::Bytes => Value::Bytes(Arc::new(
            base64::engine::general_purpose::STANDARD.decode(s).ok()?,
        )),
        _ => Value::String(Arc::new(s.to_owned())),
    })
}

/// Whether `UnstructuredToVal` takes `value` under `shape` (for `strict`).
fn takes(value: &serde_json::Value, shape: &Shape) -> bool {
    use serde_json::Value as J;
    if value.is_null() {
        return shape.nullable;
    }
    if shape.format == SchemaFormat::IntOrString {
        return matches!(value, J::String(_)) || value.as_i64().is_some();
    }
    match (shape.ty.as_deref(), value) {
        (Some("object"), J::Object(_)) | (Some("number"), J::Number(_)) | (Some("boolean"), J::Bool(_)) => {
            true
        }
        (Some("array"), J::Array(_)) => shape.items.is_some(),
        (Some("string"), J::String(s)) => strict_string(s, &shape.format).is_some(),
        (Some("integer"), J::Number(n)) => n.as_i64().is_some(),
        _ => false,
    }
}

fn bind_raw(
    value: &serde_json::Value,
    shape: Option<&Arc<Shape>>,
    strict: bool,
) -> Result<Box<dyn Val>, String> {
    let Some(shape) = shape else {
        return if strict {
            Err("no schema to bind under".to_owned())
        } else {
            Ok(from_value(json_to_cel(value)))
        };
    };
    // No `type` but preserve-unknown-fields: `unknownPreserved`, whatever
    // the JSON (a `null` is pruned or nullable before).
    if shape.ty.is_none()
        && shape.preserve_unknown
        && shape.format != SchemaFormat::IntOrString
        && !value.is_null()
    {
        return Ok(Box::new(Preserved(Arc::new(value.clone()))));
    }
    if strict && !takes(value, shape) {
        return Err(format!("invalid data for the schema: {value}"));
    }
    Ok(match value {
        serde_json::Value::Null => Box::new(CelNull),
        serde_json::Value::Array(items) => {
            bind_list(items.iter().cloned().map(Go::Raw).collect(), shape, strict)?
        }
        serde_json::Value::Object(obj) => object_to_val(obj, shape, strict)?,
        _ => from_value(scalar_to_cel(value, &shape.format)),
    })
}

/// An object under `shape`: one with declared properties (an embedded
/// resource's include its type and metadata) as an [`Object`]; a map of
/// `additionalProperties` as a plain map of bound values; one with neither as
/// an [`Object`] declaring nothing, every key absent but counted, as the
/// apiserver binds a free-form (`x-kubernetes-preserve-unknown-fields`) one.
fn object_to_val(
    obj: &serde_json::Map<String, serde_json::Value>,
    shape: &Arc<Shape>,
    strict: bool,
) -> Result<Box<dyn Val>, String> {
    let shape = if shape.embedded {
        shape.with_type_and_object_meta()
    } else {
        Arc::clone(shape)
    };
    if let Some(props) = &shape.properties {
        return object(obj, props, strict);
    }
    if let Some(additional) = &shape.additional {
        let map = obj
            .iter()
            .map(|(k, v)| {
                Ok((
                    CelMapKey::from(k.clone()),
                    element(bind_raw(v, Some(additional), strict)?),
                ))
            })
            .collect::<Result<std::collections::HashMap<_, _>, String>>()?;
        return Ok(Box::new(CelMap::from(map)));
    }
    object(obj, &BTreeMap::new(), strict)
}

/// A list of `elements` under `shape`, by its list type.
fn bind_list(elements: Vec<Go>, shape: &Arc<Shape>, strict: bool) -> Result<Box<dyn Val>, String> {
    let items = elements
        .iter()
        .map(|e| bind_go(e, shape.items.as_ref(), strict).map(element))
        .collect::<Result<Vec<_>, _>>()?;
    let bound = |items: Vec<Box<dyn Val>>| Items::new(items.into_iter().map(Item::Bound).collect());
    Ok(match &shape.list {
        ListType::Set => Box::new(SetList {
            items: bound(items),
            elements: Arc::new(elements),
            item_shape: shape.items.clone(),
            log: Arc::default(),
            wrapper: Some(next_wrapper()),
        }),
        ListType::Map(keys) => Box::new(MapList {
            items: bound(items),
            elements: Arc::new(elements),
            item_shape: shape.items.clone(),
            keys: keys
                .iter()
                .map(|k| go_escape(k).unwrap_or_else(|| k.clone()))
                .collect::<Vec<_>>()
                .into(),
        }),
        ListType::Plain => Box::new(CelList::from(items)),
    })
}

/// An [`Object`] from `obj`: a declared property (in `props`) by its JSON
/// name, anything else as an undeclared key.
fn object(
    obj: &serde_json::Map<String, serde_json::Value>,
    props: &BTreeMap<String, Arc<Shape>>,
    strict: bool,
) -> Result<Box<dyn Val>, String> {
    let mut fields = BTreeMap::new();
    let mut undeclared = BTreeMap::new();
    for (key, value) in obj {
        match props.get(key) {
            Some(shape) => {
                let field = if value.is_null() {
                    Field::Null
                } else {
                    // Only a strict (added) value can be refused.
                    bind_raw(value, Some(shape), strict).map_or(Field::Refused, Field::Bound)
                };
                fields.insert(key.clone(), field);
            }
            None => {
                undeclared.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(Box::new(Object::new(
        Arc::new(obj.clone()),
        !props.is_empty(),
        fields,
        undeclared,
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value as Json, json};

    use crate::validation::{Validator, compilation::compile_schema};

    /// Whether `rule` holds on `object` (and `old`) under `properties`, on
    /// both validation paths, which must agree.
    fn holds(properties: Json, rule: &str, object: Json, old: Option<Json>) -> bool {
        let schema = json!({
            "type": "object",
            "properties": properties,
            "x-kubernetes-validations": [{ "rule": rule }]
        });
        let validator = Validator::new();
        let plain = validator.validate(&schema, &object, old.as_ref());
        let compiled = validator.validate_compiled(&compile_schema(&schema), &object, old.as_ref());
        assert_eq!(
            plain.is_ok(),
            compiled.is_ok(),
            "{rule}: {plain:?} vs {compiled:?}"
        );
        if let Err(errors) = &plain {
            assert!(
                errors
                    .iter()
                    .all(|e| e.kind == crate::ErrorKind::ValidationFailure),
                "{rule}: {errors:?}"
            );
        }
        plain.is_ok()
    }

    fn set() -> Json {
        json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "string" } })
    }

    fn map_list() -> Json {
        json!({
            "type": "array",
            "x-kubernetes-list-type": "map",
            "x-kubernetes-list-map-keys": ["k"],
            "items": {
                "type": "object",
                "properties": { "k": { "type": "string" }, "v": { "type": "string" }, "s": set() }
            }
        })
    }

    /// Each expectation was measured on kube-apiserver v1.36.1 (kind) or with
    /// its `UnstructuredToVal` (Go, v0.36.1).
    #[test]
    fn set_lists_compare_as_sets_and_add_as_unions() {
        let props = json!({ "s": set(), "t": set() });
        let object = json!({ "s": ["b", "a"], "t": ["a", "b"] });
        for (rule, want) in [
            ("self.s == ['a', 'b']", true),
            ("self.s != ['a', 'b']", false),
            ("self.s == self.t", true),
            ("self.s == ['a', 'a']", true),
            ("self.s == ['a', 'c']", false),
            ("self.s[0] == 'b' && self.s.size() == 2 && 'a' in self.s", true),
            ("(self.s + ['b', 'c']).size() == 3", true),
            (
                "(self.s + ['b', 'c']) == ['a', 'b', 'c'] && (self.s + ['b', 'c'])[2] == 'c'",
                true,
            ),
            ("self.s.all(x, x != '')", true),
            ("self.s.filter(x, true) == ['a', 'b']", false),
            // The left operand decides. The apiserver compares a plain list
            // on the left by position, false here; `cel`'s own list is
            // unequal to a set in any order, so `['b', 'a'] == self.s`, true
            // in the apiserver, is false in kube-cel.
            ("['a', 'b'] == self.s", false),
        ] {
            assert_eq!(holds(props.clone(), rule, object.clone(), None), want, "{rule}");
        }
        #[cfg(feature = "lists")]
        assert!(holds(
            props.clone(),
            "self.s.sort() == ['a', 'b']",
            object.clone(),
            None
        ));
        let old = json!({ "s": ["a", "b"], "t": ["a", "b"] });
        assert!(holds(
            props.clone(),
            "self.s == oldSelf.s",
            object.clone(),
            Some(old.clone())
        ));
        assert!(!holds(props, "self.s != oldSelf.s", object, Some(old)));
    }

    #[test]
    fn map_lists_compare_by_key_and_add_as_merges() {
        let props = json!({ "m": map_list() });
        let object = json!({ "m": [{ "k": "b", "v": "2", "s": ["y", "x"] }, { "k": "a", "v": "1" }] });
        let old = json!({ "m": [{ "k": "a", "v": "1" }, { "k": "b", "v": "2", "s": ["x", "y"] }] });
        for (rule, want) in [
            ("self.m == oldSelf.m", true),
            ("self.m != oldSelf.m", false),
            ("self.m[0] in oldSelf.m", true),
            ("(self.m + oldSelf.m).size() == 2", true),
            (
                "(self.m + oldSelf.m)[0].k == 'b' && (self.m + oldSelf.m)[0].s == ['x', 'y']",
                true,
            ),
            ("self.m[0].k == 'b'", true),
        ] {
            assert_eq!(
                holds(props.clone(), rule, object.clone(), Some(old.clone())),
                want,
                "{rule}"
            );
        }
        // A changed value under the same key is a change.
        let changed = json!({ "m": [{ "k": "a", "v": "9" }, { "k": "b", "v": "2", "s": ["x", "y"] }] });
        assert!(!holds(props, "self.m == oldSelf.m", changed, Some(old)));
    }

    /// A `nullable` property that is `null` is absent to field access,
    /// `has()`, `.?` and `in`, but counted by `size()`, iterated, and part of
    /// `==` (the apiserver's `unstructuredMap`).
    #[test]
    fn null_properties_are_absent_but_counted() {
        let props = json!({
            "x": { "type": "integer", "nullable": true },
            "o": {
                "type": "object",
                "properties": { "x": { "type": "integer", "nullable": true }, "y": { "type": "integer", "nullable": true } }
            }
        });
        let object = json!({ "x": null, "o": { "x": null } });
        for (rule, want) in [
            ("has(self.x)", false),
            ("!has(self.x) && !self.?x.hasValue()", true),
            ("size(dyn(self.o)) == 1", true),
            ("'x' in dyn(self.o)", false),
            ("dyn(self.o).exists(k, k == 'x')", true),
            ("dyn(self.o).all(k, k in dyn(self.o))", false),
        ] {
            assert_eq!(holds(props.clone(), rule, object.clone(), None), want, "{rule}");
        }
        assert!(!holds(
            props.clone(),
            "self.o == oldSelf.o",
            object.clone(),
            Some(json!({ "o": {} }))
        ));
        assert!(holds(
            props,
            "self.o == oldSelf.o",
            object,
            Some(json!({ "o": { "y": null } }))
        ));
    }

    /// kube-cel's functions take the schema-aware values: an ordered snapshot
    /// where the result does not depend on how elements compare (`join`,
    /// `sum`, `format`), the values themselves where it does (`indexOf`,
    /// `sets.*`) or where one is returned (`first`, `last`).
    #[test]
    #[cfg(all(feature = "strings", feature = "lists", feature = "sets", feature = "format"))]
    fn functions_take_schema_aware_values() {
        let int_set =
            json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } });
        let props = json!({ "s": set(), "n": int_set, "m": map_list() });
        let object =
            json!({ "s": ["b", "a"], "n": [2, 1], "m": [{ "k": "b", "s": ["y", "x"] }, { "k": "a" }] });
        let old = json!({ "s": ["a", "b"], "n": [1, 2], "m": [{ "k": "a" }, { "k": "b", "s": ["x", "y"] }] });
        for (rule, want) in [
            ("self.s.join(',') == 'b,a'", true),
            ("self.s.indexOf('a') == 1 && self.s.lastIndexOf('b') == 0", true),
            (
                "self.n.sum() == 3 && self.n.min() == 1 && self.n.max() == 2 && !self.n.isSorted()",
                true,
            ),
            ("'%d %s'.format([self.n.size(), self.n]) == '2 [2, 1]'", true),
            (
                "self.s.first().value() == 'b' && self.s.last().value() == 'a'",
                true,
            ),
            // The element returned stays a set, and compares as one.
            ("[self.s].first().value() == ['a', 'b']", true),
            (
                "sets.contains(self.s, ['a']) && sets.equivalent(self.s, ['a', 'b']) && sets.intersects(self.s, ['a'])",
                true,
            ),
            (
                "self.m.map(x, x.k).join(',') == 'b,a' && self.m.first().value().k == 'b'",
                true,
            ),
            // An element holding a set is found through the set's equality.
            (
                "self.m.indexOf(oldSelf.m[1]) == 0 && sets.contains(self.m, [oldSelf.m[1]])",
                true,
            ),
        ] {
            assert_eq!(
                holds(props.clone(), rule, object.clone(), Some(old.clone())),
                want,
                "{rule}"
            );
        }
    }

    /// Formatting an object with a `null` property fails, as the apiserver's
    /// formatter fails to find a key it iterates.
    #[test]
    #[cfg(feature = "format")]
    fn formatting_an_object_with_a_null_property_fails() {
        let schema = json!({
            "type": "object",
            "properties": { "o": { "type": "object", "properties": { "x": { "type": "integer", "nullable": true } } } },
            "x-kubernetes-validations": [{ "rule": "'%s'.format([dyn(self.o)]) != ''" }]
        });
        let errors = Validator::new()
            .validate(&schema, &json!({ "o": { "x": null } }), None)
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == crate::ErrorKind::EvaluationError),
            "{errors:?}"
        );
    }

    /// An object's equality reaches the sets and map lists in it.
    #[test]
    fn equality_reaches_nested_sets() {
        let props = json!({ "agg": { "type": "object", "properties": { "s": set(), "m": map_list() } } });
        let object = json!({ "agg": { "s": ["b", "a"], "m": [{ "k": "b" }, { "k": "a" }] } });
        let old = json!({ "agg": { "s": ["a", "b"], "m": [{ "k": "a" }, { "k": "b" }] } });
        assert!(holds(
            props.clone(),
            "self.agg == oldSelf.agg",
            object.clone(),
            Some(old.clone())
        ));
        assert!(!holds(props, "self.agg != oldSelf.agg", object, Some(old)));
    }

    /// Whether `rule` is refused as an evaluation error on both validation
    /// paths.
    fn errs(properties: Json, rule: &str, object: Json) -> bool {
        let schema = json!({
            "type": "object",
            "properties": properties,
            "x-kubernetes-validations": [{ "rule": rule }]
        });
        let validator = Validator::new();
        [
            validator.validate(&schema, &object, None),
            validator.validate_compiled(&compile_schema(&schema), &object, None),
        ]
        .iter()
        .all(|r| {
            r.as_ref()
                .is_err_and(|e| e.iter().all(|e| e.kind == crate::ErrorKind::EvaluationError))
        })
    }

    /// `==` with a value of another kind is the apiserver's no-such-overload
    /// error, and so is an object with undeclared keys against a map that is
    /// not one of these objects; `cel`'s `equals` can only answer, so the rule
    /// is refused. `null` compares plainly, and sizes are compared first.
    #[test]
    fn equality_the_apiserver_fails_is_refused() {
        let nullable = json!({ "o": { "type": "object", "properties": { "x": { "type": "integer", "nullable": true } } } });
        for rule in [
            "self.o == dyn([1])",
            "!(self.o == dyn([1]))",
            "dyn(self.o) == 'a'",
            // Fail-closed: the apiserver's `!=` holds, and `||` absorbs the
            // error of its `==`.
            "self.o != dyn([1])",
            "self.o == dyn([1]) || true",
        ] {
            assert!(
                errs(nullable.clone(), rule, json!({ "o": { "x": null } })),
                "{rule}"
            );
        }
        assert!(holds(
            nullable.clone(),
            "self.o != null && dyn(self.o) != null",
            json!({ "o": { "x": 1 } }),
            None
        ));

        let unknown = json!({ "o": {
            "type": "object",
            "properties": { "x": { "type": "integer" } },
            "x-kubernetes-preserve-unknown-fields": true
        } });
        let object = json!({ "o": { "x": 1, "z": 2 } });
        assert!(errs(
            unknown.clone(),
            "self.o == dyn({'x': 1, 'z': 999})",
            object.clone()
        ));
        assert!(holds(
            unknown.clone(),
            "self.o != dyn({'x': 1}) && self.o == self.o",
            object,
            None
        ));

        let lists = json!({ "s": set(), "m": map_list() });
        let object = json!({ "s": ["a"], "m": [{ "k": "a" }] });
        assert!(errs(lists.clone(), "self.s == dyn({'a': 1})", object.clone()));
        assert!(errs(lists.clone(), "self.m == dyn({'a': 1})", object.clone()));
        assert!(holds(lists, "dyn(self.s) != null", object, None));
    }

    /// The apiserver iterates an object's raw keys, a nil for each undeclared
    /// one, which a comprehension binds as `null` (Go oracle, kind v1.36.1).
    #[test]
    fn undeclared_keys_iterate_as_null() {
        let unknown = json!({ "o": {
            "type": "object",
            "properties": { "x": { "type": "integer" } },
            "x-kubernetes-preserve-unknown-fields": true
        } });
        let object = json!({ "o": { "x": 1, "z": 2 } });
        for (rule, want) in [
            ("dyn(self.o).all(k, k == 'x')", false),
            ("dyn(self.o).exists(k, k == 'x')", true),
            ("dyn(self.o).exists(k, k == 'z')", false),
            ("dyn(self.o).map(k, 1).size() == 2", true),
            ("dyn(self.o).filter(k, k != null) == ['x']", true),
        ] {
            assert_eq!(holds(unknown.clone(), rule, object.clone(), None), want, "{rule}");
        }
        #[cfg(feature = "format")]
        assert!(errs(unknown, "'%s'.format([dyn(self.o)]) != ''", object));
    }

    /// A value under `additionalProperties` binds by that schema, a set as a
    /// set and an object with its null properties absent; an object with
    /// neither `properties` nor `additionalProperties` (free-form) hides
    /// every key, which still counts and iterates as `null` (Go oracle,
    /// v0.36.1).
    #[test]
    fn values_bind_by_their_schema_below_any_node() {
        let addl = |s: Json| json!({ "type": "object", "additionalProperties": s });
        let nullable =
            json!({ "type": "object", "properties": { "x": { "type": "integer", "nullable": true } } });
        let props = json!({
            "a": addl(set()),
            "o": addl(nullable),
            "l": { "type": "array", "items": addl(set()) },
            "f": { "type": "object", "x-kubernetes-preserve-unknown-fields": true },
            "g": { "type": "object", "x-kubernetes-preserve-unknown-fields": true },
            "t": { "type": "object", "additionalProperties": true }
        });
        let object = json!({
            "a": { "k": ["b", "a"] },
            "o": { "k": { "x": null } },
            "l": [{ "k": ["b", "a"] }],
            "f": { "a": 1 },
            "g": { "b": 2 },
            "t": { "a.b/c": "x" }
        });
        for (rule, want) in [
            (
                "(self.a['k'] + ['a', 'b']).size() == 2 && self.a['k'] == ['a', 'b']",
                true,
            ),
            ("has(self.o['k'].x)", false),
            ("self.l[0]['k'] == ['a', 'b']", true),
            ("has(dyn(self.f).a)", false),
            ("size(dyn(self.f)) == 1 && dyn(self.f).all(k, k == null)", true),
            // Undeclared keys compare where both sides have them.
            ("dyn(self.f) == dyn(self.g)", true),
            ("!('a.b/c' in dyn(self.t)) && size(dyn(self.t)) == 1", true),
        ] {
            assert_eq!(holds(props.clone(), rule, object.clone(), None), want, "{rule}");
        }
        assert!(errs(props, "dyn(self.f).a == 1", object));
    }

    /// The resource root and an embedded resource declare `apiVersion`,
    /// `kind` and `metadata`, the last with `name` and `generateName` only
    /// (`WithTypeAndObjectMeta`), and fill in none of them.
    #[test]
    fn resources_declare_type_and_object_meta() {
        let schema = |rule: &str| {
            json!({
                "type": "object",
                "properties": {
                    "metadata": { "type": "object" },
                    "e": { "type": "object", "x-kubernetes-embedded-resource": true, "x-kubernetes-preserve-unknown-fields": true }
                },
                "x-kubernetes-validations": [{ "rule": rule }]
            })
        };
        let object = json!({
            "metadata": { "name": "n", "labels": { "a": "b" } },
            "e": { "apiVersion": "v1", "kind": "K", "spec": { "a": 1 } }
        });
        let validator = Validator::new();
        for (rule, want) in [
            ("self.metadata.name == 'n' && size(dyn(self.metadata)) == 2", true),
            ("has(dyn(self.metadata).labels)", false),
            ("!has(self.kind) && size(dyn(self)) == 2", true),
            (
                "self.e.kind == 'K' && !has(self.e.metadata) && !has(dyn(self.e).spec)",
                true,
            ),
            ("size(dyn(self.e)) == 3", true),
        ] {
            assert_eq!(
                validator.validate(&schema(rule), &object, None).is_ok(),
                want,
                "{rule}"
            );
        }
    }

    /// Set `==` and `+` look up the `Value()` of each right-hand item among
    /// the elements as Go holds them, and an added element binds under the
    /// list's items (Go oracle, v0.36.1).
    #[test]
    fn set_equality_and_union_read_go_values() {
        let set_of =
            |items: Json| json!({ "type": "array", "x-kubernetes-list-type": "set", "items": items });
        let props = json!({
            "n": set_of(json!({ "type": "number" })),
            "d": set_of(json!({ "type": "string", "format": "duration" })),
            "e": set_of(json!({ "type": "number" })),
            "a": set_of(json!({ "type": "object", "x-kubernetes-map-type": "atomic", "properties": { "x": { "type": "integer" } } })),
            "b": set_of(json!({ "type": "string", "format": "byte" }))
        });
        let object = json!({ "n": [1, 2], "d": ["1s", "2s"], "e": [], "a": [{ "x": 1 }], "b": ["YQ=="] });
        for (rule, want) in [
            // `int64` keys against double items.
            ("self.n != self.n && size(self.n + [1.0, 2.0]) == 4", true),
            // Strings against durations.
            ("self.d != self.d", true),
            ("type((self.e + dyn([1]))[0]) == double", true),
            ("self.a[0].x == 1", true),
        ] {
            assert_eq!(holds(props.clone(), rule, object.clone(), None), want, "{rule}");
        }
        // A map or bytes as a map key: a Go panic.
        assert!(errs(props.clone(), "self.a != self.a", object.clone()));
        assert!(errs(props, "self.b != self.b", object));
    }

    /// The apiserver's set `+` adds to the receiver's cached set and appends
    /// into its slice: kube-cel computes afresh and notes the cases where
    /// that state could change the outcome (fail-closed), and only those.
    #[test]
    fn set_add_state_is_noted_where_it_could_change_the_outcome() {
        let props = json!({ "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } } });
        let object = json!({ "s": [1, 2] });
        for rule in [
            // Each read of a field is a new wrapper; the same value appended
            // at the same position changes nothing.
            "(self.s + [3]).size() == 3 && (self.s + [3]).size() == 3",
            "((self.s + [3]) + [4]).size() == 4",
        ] {
            assert!(holds(props.clone(), rule, object.clone(), None), "{rule}");
        }
        for rule in [
            // The same wrapper's cache holds 3: the apiserver's second `+`
            // appends nothing, and its `==` finds 3.
            "[self.s].all(x, (x + [3]).size() == 3 && (x + [3]).size() == 2)",
            "[self.s].all(x, (x + [3]).size() == 3 && x == [1, 3])",
            // Different values at one position of shared elements.
            "(self.s + [3]).size() == 3 && (self.s + [4]).size() == 3",
            "[self.s].all(x, (x + [3]).size() == 3 && (x + [4]).size() == 3)",
        ] {
            assert!(errs(props.clone(), rule, object.clone()), "{rule}");
        }
    }

    /// Map-list `+` binds each added item under the list's items: a null
    /// field is hidden again, and a value its schema refuses is an error.
    #[test]
    fn map_list_add_binds_under_its_items() {
        let map_of = |x: Json| {
            json!({
                "type": "array",
                "x-kubernetes-list-type": "map",
                "x-kubernetes-list-map-keys": ["k"],
                "items": { "type": "object", "properties": { "k": { "type": "string" }, "x": x } }
            })
        };
        let props = json!({
            "l": map_of(json!({ "type": "integer", "nullable": true })),
            "m": map_of(json!({ "x-kubernetes-int-or-string": true, "nullable": true }))
        });
        let object = |x: Json| json!({ "l": [], "m": [{ "k": "a", "x": x }] });
        assert!(holds(
            props.clone(),
            "!has((self.l + dyn(self.m))[0].x)",
            object(json!(null)),
            None
        ));
        assert!(errs(
            props,
            "(self.l + dyn(self.m))[0].x == 1",
            object(json!("s"))
        ));
    }

    /// The apiserver keys a map list by reading the key fields from each
    /// item's decoded map under their escaped names, so a key name escaping
    /// changes reads as nil everywhere, and four keys or more as Go's `%v` of
    /// the tuple (Go oracle, v0.36.1).
    #[test]
    fn map_list_keys_read_the_decoded_map() {
        let map_of = |keys: Json, props: Json| {
            json!({
                "type": "array",
                "x-kubernetes-list-type": "map",
                "x-kubernetes-list-map-keys": keys,
                "items": { "type": "object", "properties": props }
            })
        };
        let s = json!({ "type": "string" });
        let props = json!({
            "e": map_of(json!(["a-b"]), json!({ "a-b": s, "v": { "type": "integer" } })),
            "n": map_of(json!(["namespace"]), json!({ "namespace": s })),
            "f": map_of(json!(["a", "b", "c", "d"]), json!({ "a": s, "b": s, "c": s, "d": s }))
        });
        let object = json!({
            "e": [{ "a-b": "a", "v": 1 }, { "a-b": "b", "v": 2 }],
            "n": [{ "namespace": "a" }, { "namespace": "b" }],
            "f": [{ "a": "a b", "b": "c", "c": "d", "d": "e" }, { "a": "a", "b": "b c", "c": "d", "d": "e" }]
        });
        let old = json!({ "e": [{ "a-b": "b", "v": 22 }, { "a-b": "c", "v": 3 }], "n": [], "f": [] });
        for rule in [
            "self.e != self.e",
            "self.n != self.n",
            "self.f != self.f",
            // Both old items share the nil key, and replace the last new one.
            "size(self.e + oldSelf.e) == 2 && (self.e + oldSelf.e)[1].v == 3",
        ] {
            assert!(
                holds(props.clone(), rule, object.clone(), Some(old.clone())),
                "{rule}"
            );
        }
    }

    /// An object keeps its fields by their JSON names: a CEL name finds one
    /// through the apiserver's `Unescape`, a key iterates through `Escape`,
    /// and equality passes each raw name to `Find` on both sides, so a field
    /// whose name does not unescape to itself (`__foo__`, `a__dot__b`) is
    /// absent to it there (Go oracle, v0.36.1).
    #[test]
    fn object_fields_go_through_unescape() {
        let int = json!({ "type": "integer" });
        let props = json!({ "o": { "type": "object", "properties": {
            "__foo__": int, "a__dot__b": int, "a-b": int, "namespace": int, "x": int
        } } });
        let object = json!({ "o": { "__foo__": 1, "a__dot__b": 1, "a-b": 1, "namespace": 1, "x": 0 } });
        let old = json!({ "o": { "__foo__": 2, "a__dot__b": 2, "a-b": 1, "namespace": 1, "x": 0 } });
        for (rule, want) in [
            ("self.o == oldSelf.o", true),
            (
                "self.o.__underscores__foo__underscores__ == 1 && self.o.a__underscores__dot__underscores__b == 1 && self.o.a__dash__b == 1 && self.o.__namespace__ == 1",
                true,
            ),
            // `Unescape` leaves a name without an escape as it is.
            (
                "'__namespace__' in dyn(self.o) && 'namespace' in dyn(self.o)",
                true,
            ),
        ] {
            assert_eq!(
                holds(props.clone(), rule, object.clone(), Some(old.clone())),
                want,
                "{rule}"
            );
        }
    }

    /// cel-go's `sort()` and `distinct()` return an empty list itself, so an
    /// empty set stays a set; `sortBy` comes out a plain list (Go oracle,
    /// v0.36.1).
    #[test]
    #[cfg(feature = "lists")]
    fn empty_set_stays_a_set_through_sort_and_distinct() {
        let props = json!({ "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } } });
        let rule = "(self.s.sort() + [1, 1]).size() == 1 && (self.s.distinct() + [1, 1]).size() == 1 \
                    && (self.s.sortBy(x, x) + [1, 1]).size() == 2";
        assert!(holds(props, rule, json!({ "s": [] }), None));
    }

    /// `in` on an object with a key that is not a string is the apiserver's
    /// no-such-overload error, which an undeclared key's `null` reaches.
    #[test]
    fn membership_of_a_non_string_key_fails() {
        let unknown = json!({ "o": {
            "type": "object",
            "properties": { "x": { "type": "integer" } },
            "x-kubernetes-preserve-unknown-fields": true
        } });
        let object = json!({ "o": { "x": 1, "z": 2 } });
        for rule in [
            "dyn(self.o).exists(k, !(k in dyn(self.o)))",
            "dyn(self.o).all(k, k in dyn(self.o)) == false",
            "!(1 in dyn(self.o))",
        ] {
            assert!(errs(unknown.clone(), rule, object.clone()), "{rule}");
        }
        assert!(holds(
            unknown,
            "'x' in dyn(self.o) && !('z' in dyn(self.o))",
            object,
            None
        ));
    }

    /// The apiserver evaluates the rules of a node on one activation, so a
    /// set node's `self` is one wrapper for all of them: a `+` in one rule
    /// grows the cached set a later rule's `+` or `==` reads.
    #[test]
    fn set_add_state_outlives_a_rule_on_the_same_node() {
        let schema = |rules: &[&str]| {
            json!({
                "type": "object",
                "properties": { "s": {
                    "type": "array",
                    "x-kubernetes-list-type": "set",
                    "items": { "type": "integer" },
                    "x-kubernetes-validations": rules.iter().map(|r| json!({ "rule": r })).collect::<Vec<_>>()
                } }
            })
        };
        let object = json!({ "s": [1, 2] });
        let validator = Validator::new();
        for second in ["(self + [3]).size() == 3", "self != [1, 3]"] {
            let errors = validator
                .validate(&schema(&["(self + [3]).size() == 3", second]), &object, None)
                .unwrap_err();
            assert_eq!(errors.len(), 1, "{second}: {errors:?}");
            assert_eq!(errors[0].rule, second);
            assert_eq!(errors[0].kind, crate::ErrorKind::EvaluationError, "{second}");
        }
        // A key the first rule did not add, or another value at the same
        // position in another rule, changes nothing.
        assert!(
            validator
                .validate(
                    &schema(&["(self + [3]).size() == 3", "(self + [4]).size() == 3"]),
                    &object,
                    None
                )
                .is_ok()
        );
    }

    /// A map list keys a right-hand plain map (an `additionalProperties` map,
    /// or a CEL-built one) as the apiserver would only for the former: noted.
    #[test]
    fn map_list_keying_a_plain_map_is_noted() {
        let props = json!({
            "m": map_list(),
            "r": { "type": "array", "items": { "type": "object", "additionalProperties": { "type": "string" } } }
        });
        let object = json!({ "m": [{ "k": "a" }], "r": [{ "k": "a" }] });
        assert!(errs(props.clone(), "self.m != dyn(self.r)", object.clone()));
        // Sizes are compared before any key.
        assert!(holds(props, "self.m != dyn([])", object, None));
    }

    /// An element `+` added, and a field of one, binds when first read: one
    /// its schema refuses is an error where read, not before (Go oracle,
    /// v0.36.1). Iteration can only take a value, so it notes one.
    #[test]
    fn added_elements_bind_when_read() {
        let int_set =
            json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } });
        let map_of = |x: Json| {
            json!({
                "type": "array",
                "x-kubernetes-list-type": "map",
                "x-kubernetes-list-map-keys": ["k"],
                "items": { "type": "object", "properties": { "k": { "type": "string" }, "x": x } }
            })
        };
        let props = json!({
            "s": int_set,
            "l": map_of(json!({ "type": "integer", "nullable": true })),
            "m": map_of(json!({ "x-kubernetes-int-or-string": true, "nullable": true }))
        });
        let object = json!({ "s": [], "l": [], "m": [{ "k": "a", "x": "s" }] });
        for rule in [
            "(self.s + dyn(['a'])).size() == 1",
            "(self.s + dyn(['a']))[0] == dyn('a') || true",
            "(self.l + dyn(self.m))[0].k == 'a'",
            "has((self.l + dyn(self.m))[0].x) || true",
        ] {
            assert!(holds(props.clone(), rule, object.clone(), None), "{rule}");
        }
        for rule in [
            "(self.s + dyn(['a']))[0] == dyn('a')",
            "1 in (self.s + dyn(['a']))",
            "(self.l + dyn(self.m))[0].x == 1",
            // Fail-closed: iteration cannot carry the error value.
            "(self.s + dyn(['a'])).all(x, true)",
        ] {
            assert!(errs(props.clone(), rule, object.clone()), "{rule}");
        }
    }

    /// A value under a node with no `type` but preserve-unknown-fields is the
    /// apiserver's `unknownPreserved` (Go oracle, v0.36.1).
    #[test]
    fn typeless_preserved_values_compare_and_nothing_else() {
        let props = json!({ "u": { "x-kubernetes-preserve-unknown-fields": true } });
        for (u, rule, want) in [
            (json!("x"), "dyn(self).u == 'x' && dyn(self).u != 'y'", true),
            (json!(1), "dyn(self).u == 1 && dyn(self).u != 1.0", true),
            (
                json!({ "a": [1] }),
                "dyn(self).u == dyn(self).u && !has(dyn(self).u.a)",
                true,
            ),
            (json!({ "a": 1 }), "!dyn(self).u.?a.hasValue()", true),
            // On the right, another type's `==`: unequal.
            (json!("x"), "'x' == dyn(self).u", false),
        ] {
            assert_eq!(
                holds(props.clone(), rule, json!({ "u": u }), None),
                want,
                "{rule}"
            );
        }
        for (u, rule) in [
            (json!("x"), "dyn(self).u.size() == 1"),
            (json!(1), "dyn(self).u + 1 == 2"),
            (json!([1]), "dyn(self).u[0] == 1"),
            (json!("x"), "type(dyn(self).u) != string"),
            // A plain list: a list field's decoded `[]interface{}` (deeply
            // equal) or a CEL-built list (never), which kube-cel cannot tell.
            (json!([1]), "dyn(self).u == dyn([1])"),
        ] {
            assert!(errs(props.clone(), rule, json!({ "u": u })), "{rule}");
        }
    }

    /// A uint, `null` or duration one rule's `+` caches on a set node's
    /// `self` is found by a later rule's `+` (noted, as for any key), and a
    /// timestamp, which kube-cel does not key, may be.
    #[test]
    fn opaque_keys_added_in_one_rule_are_noted_in_the_next() {
        let schema = |value: &str| {
            json!({
                "type": "object",
                "properties": { "s": {
                    "type": "array",
                    "x-kubernetes-list-type": "set",
                    "items": { "type": "integer" },
                    "x-kubernetes-validations": [
                        { "rule": format!("(self + dyn([{value}])).size() == 3") },
                        { "rule": format!("(self + dyn([{value}])).size() == 3") }
                    ]
                } }
            })
        };
        for value in [
            "1u",
            "null",
            "duration('1s')",
            "timestamp('2020-01-01T00:00:00Z')",
        ] {
            let errors = Validator::new()
                .validate(&schema(value), &json!({ "s": [1, 2] }), None)
                .unwrap_err();
            assert!(
                errors.len() == 1 && errors[0].kind == crate::ErrorKind::EvaluationError,
                "{value}: {errors:?}"
            );
        }
        // A uint is a map key of its own: 2u is not the 1u cached.
        let mut other = schema("1u");
        other["properties"]["s"]["x-kubernetes-validations"][1]["rule"] =
            json!("(self + dyn([2u])).size() == 3");
        assert!(
            Validator::new()
                .validate(&other, &json!({ "s": [1, 2] }), None)
                .is_ok()
        );
    }

    /// A set held in an `additionalProperties` map or a list is bound afresh
    /// on each `Find` or `Get`: no `+` state carries from one read, or rule,
    /// to the next, while a variable holding one keeps it (Go oracle,
    /// v0.36.1).
    #[test]
    fn sets_read_through_maps_and_lists_are_new_wrappers() {
        let int_set =
            json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } });
        let schema = |rules: &[&str]| {
            json!({
                "type": "object",
                "properties": {
                    "d": { "type": "object", "additionalProperties": int_set },
                    "l": { "type": "array", "items": int_set }
                },
                "x-kubernetes-validations": rules.iter().map(|r| json!({ "rule": r })).collect::<Vec<_>>()
            })
        };
        let object = json!({ "d": { "x": [1, 2] }, "l": [[1, 2]] });
        let validator = Validator::new();
        for rules in [
            &[
                "(self.d['x'] + [3]).size() == 3",
                "(self.d['x'] + [3]).size() == 3",
            ][..],
            &["(self.l[0] + [3]).size() == 3", "(self.l[0] + [3]).size() == 3"][..],
            &[
                "self.l.all(x, (x + [3]).size() == 3)",
                "self.l.all(x, (x + [3]).size() == 3)",
            ][..],
        ] {
            assert!(
                validator.validate(&schema(rules), &object, None).is_ok(),
                "{rules:?}"
            );
        }
        let held = schema(&["[self.d['x']].all(x, (x + [3]).size() == 3 && (x + [3]).size() == 3)"]);
        assert!(validator.validate(&held, &object, None).is_err());
    }

    /// An `optionalOldSelf` rule's `oldSelf.value()` is the old value's
    /// `Value()` bound again by cel-go's `NativeToValue`: plain JSON for a
    /// list or map (a set a plain list, `null` and undeclared fields
    /// present), and its `messageExpression` sees the node's plain `oldSelf`
    /// (Go oracle, v0.36.1).
    #[test]
    fn optional_old_self_holds_the_old_value_as_plain_json() {
        let int_set =
            json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } });
        let props = json!({
            "s": int_set,
            "o": {
                "type": "object",
                "properties": { "x": { "type": "integer", "nullable": true }, "n": { "type": "number" } },
                "x-kubernetes-preserve-unknown-fields": true
            }
        });
        let schema = |rule: &str, message_expression: Option<&str>| {
            let mut rule = json!({ "rule": rule, "optionalOldSelf": true });
            if let Some(m) = message_expression {
                rule["messageExpression"] = json!(m);
            }
            json!({ "type": "object", "properties": props, "x-kubernetes-validations": [rule] })
        };
        let old = json!({ "s": [1, 2], "o": { "x": null, "n": 1, "z": 2 } });
        let object = json!({ "s": [1, 4], "o": { "x": null, "n": 1, "z": 2 } });
        let validator = Validator::new();
        for (rule, want) in [
            // A plain list: `+` concatenates.
            (
                "!oldSelf.hasValue() || (oldSelf.value().s + dyn([1u, 1u])).size() == 4",
                true,
            ),
            ("!oldSelf.hasValue() || oldSelf.value().s != [2, 1]", true),
            (
                "!oldSelf.hasValue() || (has(oldSelf.value().o.x) && type(oldSelf.value().o.n) == int \
                 && dyn(oldSelf.value().o).z == 2)",
                true,
            ),
        ] {
            assert_eq!(
                validator
                    .validate(&schema(rule, None), &object, Some(&old))
                    .is_ok(),
                want,
                "{rule}"
            );
        }
        // The message compiles against the rule's optional `oldSelf`, so it
        // reads the field through `dyn`, as the apiserver registers it; it
        // evaluates on the node's activation, `oldSelf` the plain old object.
        let errors = validator
            .validate(
                &schema(
                    "!oldSelf.hasValue()",
                    Some("size(dyn(oldSelf).s) == 2 ? 'old 2' : 'other'"),
                ),
                &object,
                Some(&old),
            )
            .unwrap_err();
        assert_eq!(errors[0].message, "old 2");
    }
}
