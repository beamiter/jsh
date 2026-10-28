//! Shell variables: values, attributes and bash's dynamic scope chain.
//!
//! Bash resolves every variable reference through a stack of variable
//! contexts — one per running function, plus the temporary environment that
//! a command's prefix assignments (`IFS=, read ...`) create — over the global
//! table. A reference finds the innermost context that holds the name, even
//! one that merely *declares* it (`local x` with no value shadows a global `x`
//! as unset), and an assignment to a name that no context holds creates a
//! global. Only `local`, `declare` and `typeset` inside a function make
//! locals. Getting this wrong is not cosmetic: a function that sets a global
//! (`f() { x=1; }`) is how most scripts return values.
//!
//! Every variable is one [`Variable`]: a [`VarValue`] (scalar, sparse indexed
//! array, associative array, or declared-but-unset) and its [`VarAttrs`].
//! The process environment is kept equal to the exported variables that are
//! visible from the current scope, because external commands are started
//! with `execvp`, which hands the child the process environment.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::environment::{remove_env_checked, set_env_checked, ShellState};

/// Names the process environment may hold: everything inherited at startup
/// plus everything the shell has exported since. The shell only adds to its
/// environment through [`ShellState::sync_env`], so a name outside this set
/// is certainly absent and needs no `getenv` to prove it. Removals leave the
/// set a superset, which only costs a lookup.
static ENV_NAMES: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);

fn with_env_names<T>(f: impl FnOnce(&mut HashSet<String>) -> T) -> T {
    let mut guard = ENV_NAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let names = guard.get_or_insert_with(|| {
        std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .collect()
    });
    f(names)
}

fn may_be_in_env(name: &str) -> bool {
    with_env_names(|names| names.contains(name))
}

fn note_env_name(name: &str) {
    with_env_names(|names| {
        if !names.contains(name) {
            names.insert(name.to_string());
        }
    });
}

/// Bash's limit on following a chain of namerefs (`NAMEREF_MAX`). A longer
/// chain is reported as circular rather than followed forever.
const MAX_NAMEREF_DEPTH: usize = 8;

/// The variable attributes `declare` manages, as one bit set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct VarAttrs(u16);

impl VarAttrs {
    pub const NONE: Self = Self(0);
    /// `-x`: passed to the environment of executed commands.
    pub const EXPORT: Self = Self(1 << 0);
    /// `-r`: may not be assigned or unset.
    pub const READONLY: Self = Self(1 << 1);
    /// `-i`: assignments are evaluated as arithmetic.
    pub const INTEGER: Self = Self(1 << 2);
    /// `-l`: assigned values are lowercased.
    pub const LOWER: Self = Self(1 << 3);
    /// `-u`: assigned values are uppercased.
    pub const UPPER: Self = Self(1 << 4);
    /// `-c`: assigned values are capitalized (first character upper, rest lower).
    pub const CAPITALIZE: Self = Self(1 << 5);
    /// `-n`: the value names another variable that references resolve to.
    pub const NAMEREF: Self = Self(1 << 6);
    /// `-a`: indexed array.
    pub const ARRAY: Self = Self(1 << 7);
    /// `-A`: associative array.
    pub const ASSOC: Self = Self(1 << 8);
    /// `-t`: trace (only meaningful for functions; kept so `declare -p` echoes it).
    pub const TRACE: Self = Self(1 << 9);

    /// The case-conversion attributes, at most one of which is ever set.
    pub const CASE: Self = Self(Self::LOWER.0 | Self::UPPER.0 | Self::CAPITALIZE.0);

    /// Attribute letters in the order `declare -p` prints them.
    const LETTERS: [(char, VarAttrs); 10] = [
        ('a', Self::ARRAY),
        ('A', Self::ASSOC),
        ('i', Self::INTEGER),
        ('n', Self::NAMEREF),
        ('r', Self::READONLY),
        ('t', Self::TRACE),
        ('x', Self::EXPORT),
        ('c', Self::CAPITALIZE),
        ('l', Self::LOWER),
        ('u', Self::UPPER),
    ];

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The attribute for one `declare` option letter.
    pub fn from_letter(letter: char) -> Option<Self> {
        Self::LETTERS
            .iter()
            .find(|(l, _)| *l == letter)
            .map(|(_, attr)| *attr)
    }

    /// Letters as `declare -p` prints them (`declare -irx n=...` → `"irx"`).
    pub fn letters(self) -> String {
        Self::LETTERS
            .iter()
            .filter(|(_, attr)| self.contains(*attr))
            .map(|(l, _)| *l)
            .collect()
    }

    /// Parse [`Self::letters`] back (session snapshots store attributes so).
    pub fn from_letters(text: &str) -> Option<Self> {
        let mut attrs = Self::NONE;
        for letter in text.chars() {
            attrs.insert(Self::from_letter(letter)?);
        }
        Some(attrs)
    }
}

/// A bash indexed array. Bash arrays are sparse: `unset 'a[1]'` leaves a hole
/// that `${!a[@]}` shows, and `a=([2]=x [5]=y)` has exactly two elements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedArray(BTreeMap<i64, String>);

impl IndexedArray {
    pub fn new() -> Self {
        Self::default()
    }

    /// A dense array `[0]=values[0] [1]=values[1] ...`.
    pub fn from_values(values: Vec<String>) -> Self {
        Self(
            values
                .into_iter()
                .enumerate()
                .map(|(i, v)| (i as i64, v))
                .collect(),
        )
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, index: i64) -> Option<&str> {
        self.0.get(&index).map(String::as_str)
    }

    pub fn set(&mut self, index: i64, value: String) {
        self.0.insert(index, value);
    }

    pub fn remove(&mut self, index: i64) -> Option<String> {
        self.0.remove(&index)
    }

    pub fn max_index(&self) -> Option<i64> {
        self.0.keys().next_back().copied()
    }

    /// The index `arr+=(x)` appends at: one past the highest set element.
    /// `None` once the array already reaches the largest representable index.
    pub fn next_index(&self) -> Option<i64> {
        match self.max_index() {
            Some(max) => max.checked_add(1),
            None => Some(0),
        }
    }

    /// Resolve a subscript that may count back from the end: `-1` is the
    /// highest set index, as in bash. `None` when it reaches before index 0.
    pub fn resolve(&self, index: i64) -> Option<i64> {
        if index >= 0 {
            return Some(index);
        }
        let end = self.max_index().map_or(0, |max| max.saturating_add(1));
        let resolved = end.checked_add(index)?;
        (resolved >= 0).then_some(resolved)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (i64, &String)> {
        self.0.iter().map(|(i, v)| (*i, v))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.0.values()
    }

    pub fn indices(&self) -> impl DoubleEndedIterator<Item = i64> + '_ {
        self.0.keys().copied()
    }

    /// True when the indices are exactly `0..len` (no holes).
    pub fn is_dense(&self) -> bool {
        self.max_index()
            .is_none_or(|max| max as usize + 1 == self.len())
    }
}

/// Bash's initial hash-table size for associative arrays (`ASSOC_HASH_BUCKETS`).
const ASSOC_INITIAL_BUCKETS: u64 = 1024;

/// A bash associative array.
///
/// Bash lists an associative array (`${!m[@]}`, `"${m[@]}"`, `declare -p`) in
/// the order of its hash table: bucket by bucket, each bucket's newest entry
/// first. Scripts print these and tests compare that output, so the order is
/// reproduced rather than left to Rust's randomized `HashMap`. Each entry
/// remembers bash's hash of its key and an insertion sequence number; listing
/// sorts by `(bucket, newest first)`. When bash grows the table (once the
/// entry count reaches twice the bucket count, to four times the buckets) it
/// re-inserts every item in listing order at the head of its new bucket,
/// which `Self::grow` mirrors by renumbering the sequence.
#[derive(Debug, Clone)]
pub struct AssocArray {
    entries: HashMap<String, AssocEntry>,
    buckets: u64,
    next_seq: u64,
}

#[derive(Debug, Clone)]
struct AssocEntry {
    value: String,
    hash: u32,
    seq: u64,
}

impl Default for AssocArray {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            buckets: ASSOC_INITIAL_BUCKETS,
            next_seq: 0,
        }
    }
}

impl PartialEq for AssocArray {
    /// Two associative arrays are equal when they map the same keys to the
    /// same values; the listing order is a property of their history.
    fn eq(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self
                .entries
                .iter()
                .all(|(k, e)| other.entries.get(k).is_some_and(|o| o.value == e.value))
    }
}

/// Bash's `hash_string`: 32-bit FNV-1 (multiply, then xor) over the bytes.
fn bash_hash(key: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in key.bytes() {
        hash = hash.wrapping_mul(16_777_619);
        hash ^= u32::from(byte);
    }
    hash
}

impl AssocArray {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|e| e.value.as_str())
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Insert or replace. Replacing keeps the entry's position, as in bash.
    pub fn insert(&mut self, key: String, value: String) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.value = value;
            return;
        }
        if self.entries.len() as u64 >= self.buckets.saturating_mul(2) {
            self.grow();
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let hash = bash_hash(&key);
        self.entries.insert(key, AssocEntry { value, hash, seq });
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.entries.remove(key).map(|e| e.value)
    }

    fn ordered(&self) -> Vec<(&String, &AssocEntry)> {
        let mask = self.buckets - 1;
        let mut items: Vec<_> = self.entries.iter().collect();
        items.sort_by_key(|(_, e)| (u64::from(e.hash) & mask, std::cmp::Reverse(e.seq)));
        items
    }

    fn grow(&mut self) {
        // Bash walks the old table in listing order and pushes each item onto
        // the head of its new bucket, so the item walked last leads it.
        let order: Vec<String> = self.ordered().into_iter().map(|(k, _)| k.clone()).collect();
        for key in order {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.seq = self.next_seq;
                self.next_seq += 1;
            }
        }
        self.buckets = self.buckets.saturating_mul(4);
    }

    /// `(key, value)` pairs in bash's listing order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.ordered()
            .into_iter()
            .map(|(k, e)| (k.as_str(), e.value.as_str()))
    }

    pub fn keys(&self) -> Vec<String> {
        self.iter().map(|(k, _)| k.to_string()).collect()
    }

    pub fn values(&self) -> Vec<String> {
        self.iter().map(|(_, v)| v.to_string()).collect()
    }
}

/// What a variable holds.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VarValue {
    /// Declared without a value (`local x`, `declare -i n`, `export X` before
    /// any assignment), or a local unset inside its own function. It still
    /// shadows outer variables of the same name and keeps its attributes.
    #[default]
    Unset,
    Scalar(String),
    Indexed(IndexedArray),
    Assoc(AssocArray),
}

/// One shell variable.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Variable {
    pub value: VarValue,
    pub attrs: VarAttrs,
}

impl Variable {
    pub fn scalar(value: impl Into<String>) -> Self {
        Self {
            value: VarValue::Scalar(value.into()),
            attrs: VarAttrs::NONE,
        }
    }

    /// A scalar with the export attribute, the shape of an inherited
    /// environment entry.
    pub fn exported(value: impl Into<String>) -> Self {
        Self {
            value: VarValue::Scalar(value.into()),
            attrs: VarAttrs::EXPORT,
        }
    }

    pub fn is_set(&self) -> bool {
        !matches!(self.value, VarValue::Unset)
    }

    /// Indexed array, set or merely declared with `-a`.
    pub fn is_indexed(&self) -> bool {
        matches!(self.value, VarValue::Indexed(_)) || self.attrs.contains(VarAttrs::ARRAY)
    }

    /// Associative array, set or merely declared with `-A`.
    pub fn is_assoc(&self) -> bool {
        matches!(self.value, VarValue::Assoc(_)) || self.attrs.contains(VarAttrs::ASSOC)
    }

    pub fn is_array(&self) -> bool {
        self.is_indexed() || self.is_assoc()
    }

    /// `$name` of this variable: the scalar, or element 0 of an array (bash
    /// treats `$arr` as `${arr[0]}` and `$assoc` as `${assoc[0]}`).
    pub fn scalar_value(&self) -> Option<&str> {
        match &self.value {
            VarValue::Scalar(s) => Some(s),
            VarValue::Indexed(a) => a.get(0),
            VarValue::Assoc(m) => m.get("0"),
            VarValue::Unset => None,
        }
    }

    /// Bash's attribute letters for this variable, including `a`/`A` for a
    /// value that is an array even without the declared attribute.
    pub fn attr_letters(&self) -> String {
        let mut attrs = self.attrs;
        match self.value {
            VarValue::Indexed(_) => attrs.insert(VarAttrs::ARRAY),
            VarValue::Assoc(_) => attrs.insert(VarAttrs::ASSOC),
            _ => {}
        }
        attrs.letters()
    }
}

/// One function's (or one command's temporary) variables.
#[derive(Debug, Clone, Default)]
pub struct VarScope {
    pub vars: HashMap<String, Variable>,
    /// The scope holds a command's prefix assignments (`IFS=, read ...`, or
    /// `V=x f` for a function call) rather than a function's locals. Such a
    /// scope is searched like any other, but `local` declares into the
    /// nearest *function* scope beneath it.
    pub temporary: bool,
}

/// Where a variable lives in the scope chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Global,
    /// Index into [`ShellState::local_vars_stack`].
    Local(usize),
}

/// The variable (and possibly array element) a name refers to once namerefs
/// are followed. `declare -n e='arr[1]'` makes `e` refer to one element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarRef {
    pub name: String,
    pub subscript: Option<String>,
}

/// Split `name[subscript]` into its parts; `None` for anything else.
pub fn split_subscripted(text: &str) -> Option<(&str, &str)> {
    let open = text.find('[')?;
    let inner = text.strip_suffix(']')?.get(open + 1..)?;
    let name = &text[..open];
    crate::environment::is_valid_identifier(name).then_some((name, inner))
}

/// One element of a compound assignment (`arr=(a [5]=b [k]+=c)`) after word
/// expansion. `key` is the expanded subscript text, still to be evaluated
/// arithmetically for an indexed array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundElement {
    pub key: Option<String>,
    pub value: String,
    /// `[k]+=v`: append to the element instead of replacing it.
    pub append: bool,
}

/// Which kind of array a compound assignment builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayKind {
    Indexed,
    Assoc,
}

/// The dynamic special variables' state.
#[derive(Debug)]
pub struct DynamicVars {
    /// `RANDOM`'s generator state (bash's `rseed`) and the value it last
    /// returned; bash never hands out the same value twice in a row.
    random_seed: std::cell::Cell<u32>,
    random_last: std::cell::Cell<u32>,
    /// The process that seeded the generator. A subshell (a fork) reseeds on
    /// first use, as bash does, so two command substitutions do not produce
    /// the same "random" numbers.
    random_pid: std::cell::Cell<u32>,
    /// `SECONDS`: the wall-clock second it was last set, and the value set.
    seconds_base: i64,
    seconds_assigned: i64,
    /// Names whose special meaning `unset` removed (bash: "If RANDOM is
    /// unset, it loses its special properties, even if it is subsequently
    /// reset").
    disabled: Vec<&'static str>,
}

/// Variables whose value is computed on every reference.
const DYNAMIC_NAMES: [&str; 6] = [
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "BASHPID",
];

fn now_epoch() -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

impl DynamicVars {
    pub fn new() -> Self {
        let pid = std::process::id();
        let now = now_epoch();
        Self {
            random_seed: std::cell::Cell::new(Self::time_seed(pid)),
            random_last: std::cell::Cell::new(0),
            random_pid: std::cell::Cell::new(pid),
            seconds_base: now.as_secs() as i64,
            seconds_assigned: 0,
            disabled: Vec::new(),
        }
    }

    /// Bash seeds from the time of day and the pid.
    fn time_seed(pid: u32) -> u32 {
        let now = now_epoch();
        (now.as_secs() as u32) ^ now.subsec_micros() ^ pid
    }

    /// Bash's `intrand32`: the Park–Miller "minimal standard" generator.
    fn next_seed(last: u32) -> u32 {
        let last = if last == 0 { 123_459_876 } else { last } as i64;
        let high = last / 127_773;
        let low = last - 127_773 * high;
        let t = 16_807 * low - 2_836 * high;
        (if t < 0 { t + 0x7fff_ffff } else { t }) as u32
    }

    /// Bash's `get_random_number`, at its default compatibility level: the
    /// same seed yields the same sequence bash would produce.
    fn random(&self) -> u32 {
        let pid = std::process::id();
        if pid != self.random_pid.get() {
            self.random_seed.set(Self::time_seed(pid));
            self.random_last.set(0);
            self.random_pid.set(pid);
        }
        loop {
            let seed = Self::next_seed(self.random_seed.get());
            self.random_seed.set(seed);
            let value = ((seed >> 16) ^ (seed & 0xffff)) & 0x7fff;
            if value != self.random_last.get() {
                self.random_last.set(value);
                return value;
            }
        }
    }

    fn seed_random(&self, seed: u32) {
        self.random_seed.set(seed);
        self.random_last.set(0);
        self.random_pid.set(std::process::id());
    }

    fn is_active(&self, name: &str) -> bool {
        DYNAMIC_NAMES.contains(&name) && !self.disabled.contains(&name)
    }
}

impl Default for DynamicVars {
    fn default() -> Self {
        Self::new()
    }
}

/// 32 bits from the operating system's random source, for `SRANDOM`.
fn os_random_u32() -> u32 {
    let mut buf = [0u8; 4];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: getrandom writes at most `buf.len()` bytes into `buf`.
        let n = unsafe { nix::libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n == buf.len() as isize {
            return u32::from_ne_bytes(buf);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut buf))
            .is_ok()
        {
            return u32::from_ne_bytes(buf);
        }
    }
    // If the OS random source is unavailable, preserve the existing fallback
    // rather than failing a shell expansion.
    let now = now_epoch();
    now.subsec_nanos() ^ (now.as_secs() as u32) ^ std::process::id()
}

/// The kernel's host name (`gethostname`), which bash's `HOSTNAME` reports.
fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    let rc = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len]).ok().map(str::to_string)
}

/// Do two paths name the same directory (same device and inode)?
fn same_directory(a: &str, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.is_dir() && a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Lowercase, uppercase or capitalize a value per its case attribute.
fn apply_case(attrs: VarAttrs, value: &str) -> String {
    if attrs.contains(VarAttrs::UPPER) {
        value.to_uppercase()
    } else if attrs.contains(VarAttrs::LOWER) {
        value.to_lowercase()
    } else if attrs.contains(VarAttrs::CAPITALIZE) {
        let mut chars = value.chars();
        match chars.next() {
            Some(first) => first
                .to_uppercase()
                .chain(chars.as_str().to_lowercase().chars())
                .collect(),
            None => String::new(),
        }
    } else {
        value.to_string()
    }
}

impl ShellState {
    // ------------------------------------------------------------------
    // Scope chain
    // ------------------------------------------------------------------

    /// The innermost scope that holds `name` (set or merely declared).
    pub fn slot_of(&self, name: &str) -> Option<Slot> {
        for (i, scope) in self.local_vars_stack.iter().enumerate().rev() {
            if scope.vars.contains_key(name) {
                return Some(Slot::Local(i));
            }
        }
        self.globals.contains_key(name).then_some(Slot::Global)
    }

    pub fn var_at(&self, slot: Slot, name: &str) -> Option<&Variable> {
        match slot {
            Slot::Global => self.globals.get(name),
            Slot::Local(i) => self.local_vars_stack.get(i)?.vars.get(name),
        }
    }

    pub fn var_at_mut(&mut self, slot: Slot, name: &str) -> Option<&mut Variable> {
        match slot {
            Slot::Global => self.globals.get_mut(name),
            Slot::Local(i) => self.local_vars_stack.get_mut(i)?.vars.get_mut(name),
        }
    }

    fn table_mut(&mut self, slot: Slot) -> &mut HashMap<String, Variable> {
        match slot {
            Slot::Local(i) if i < self.local_vars_stack.len() => &mut self.local_vars_stack[i].vars,
            _ => &mut self.globals,
        }
    }

    /// Index of the innermost function scope, skipping temporary scopes.
    pub fn function_scope(&self) -> Option<usize> {
        self.local_vars_stack
            .iter()
            .rposition(|scope| !scope.temporary)
    }

    /// True while a shell function is running (where `local` is allowed).
    pub fn in_function(&self) -> bool {
        self.function_scope().is_some()
    }

    /// The variable `name` refers to from here, without following namerefs.
    pub fn find_variable(&self, name: &str) -> Option<&Variable> {
        self.slot_of(name).and_then(|slot| self.var_at(slot, name))
    }

    /// Follow `name` through namerefs to the variable (or element) it names.
    ///
    /// A nameref that is unset or empty refers to itself, which is how
    /// `declare -n r; r=x` stores `x` as the reference. A chain longer than
    /// bash's limit is reported as circular.
    pub fn resolve_nameref(&self, name: &str) -> Result<VarRef, String> {
        let mut current = name.to_string();
        for _ in 0..=MAX_NAMEREF_DEPTH {
            let target = match self.find_variable(&current) {
                Some(var) if var.attrs.contains(VarAttrs::NAMEREF) => match &var.value {
                    VarValue::Scalar(target) if !target.is_empty() => Some(target.clone()),
                    _ => None,
                },
                _ => None,
            };
            let Some(target) = target else {
                return Ok(VarRef {
                    name: current,
                    subscript: None,
                });
            };
            if let Some((base, subscript)) = split_subscripted(&target) {
                return Ok(VarRef {
                    name: base.to_string(),
                    subscript: Some(subscript.to_string()),
                });
            }
            if target == name {
                break;
            }
            current = target;
        }
        Err(format!("{name}: circular name reference"))
    }

    /// The variable `name` refers to after following namerefs. A reference
    /// to one array element yields the array.
    pub fn resolved_variable(&self, name: &str) -> Option<&Variable> {
        let var = self.find_variable(name)?;
        if !var.attrs.contains(VarAttrs::NAMEREF) {
            return Some(var);
        }
        let target = self.resolve_nameref(name).ok()?;
        self.find_variable(&target.name)
    }

    // ------------------------------------------------------------------
    // Reading
    // ------------------------------------------------------------------

    /// The stored value of `$name`: a scalar, or element 0 of an array.
    /// Follows namerefs. Dynamic specials such as `RANDOM` are not stored;
    /// [`Self::var_value`] computes them.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        if name == "?" {
            return None; // handled by expand
        }
        // `LINENO`, `BASH_COMMAND` and `$_` change with every command; they
        // are read from the call stack rather than stored (and exported) as
        // ordinary variables.
        if let Some(value) = self.call_stack.dynamic_var(name) {
            return Some(value);
        }
        // Most variables are not namerefs: answer without a second lookup.
        let var = self.find_variable(name)?;
        if !var.attrs.contains(VarAttrs::NAMEREF) {
            return var.scalar_value();
        }
        let target = self.resolve_nameref(name).ok()?;
        let var = self.find_variable(&target.name)?;
        match target.subscript {
            None => var.scalar_value(),
            Some(subscript) => match &var.value {
                VarValue::Indexed(a) => {
                    let index = self.literal_index(&subscript)?;
                    a.get(a.resolve(index)?)
                }
                VarValue::Assoc(m) => m.get(&subscript),
                VarValue::Scalar(s) => (self.literal_index(&subscript)? == 0).then_some(s.as_str()),
                VarValue::Unset => None,
            },
        }
    }

    /// The value `$name` expands to: [`Self::get_var`], or the computed value
    /// of a dynamic special (`RANDOM`, `SRANDOM`, `SECONDS`, `EPOCHSECONDS`,
    /// `EPOCHREALTIME`, `BASHPID`) unless a local shadows it or `unset`
    /// removed its special meaning.
    pub fn var_value(&self, name: &str) -> Option<Cow<'_, str>> {
        if let Some(value) = self.dynamic_value(name) {
            return Some(Cow::Owned(value));
        }
        self.get_var(name).map(Cow::Borrowed)
    }

    /// Is `name` currently computed rather than stored?
    pub fn is_dynamic_var(&self, name: &str) -> bool {
        self.dynamic.is_active(name) && !matches!(self.slot_of(name), Some(Slot::Local(_)))
    }

    fn dynamic_value(&self, name: &str) -> Option<String> {
        if !self.is_dynamic_var(name) {
            return None;
        }
        Some(match name {
            "RANDOM" => self.dynamic.random().to_string(),
            "SRANDOM" => os_random_u32().to_string(),
            "SECONDS" => {
                let elapsed = now_epoch().as_secs() as i64 - self.dynamic.seconds_base;
                self.dynamic
                    .seconds_assigned
                    .wrapping_add(elapsed)
                    .to_string()
            }
            "EPOCHSECONDS" => now_epoch().as_secs().to_string(),
            "EPOCHREALTIME" => {
                let now = now_epoch();
                format!("{}.{:06}", now.as_secs(), now.subsec_micros())
            }
            "BASHPID" => std::process::id().to_string(),
            _ => return None,
        })
    }

    /// An integer subscript written literally (`3`, `-1`), or a variable
    /// holding one. Callers with a general arithmetic subscript evaluate it
    /// themselves; this is the read-only fallback `&self` lookups can afford.
    fn literal_index(&self, subscript: &str) -> Option<i64> {
        let text = subscript.trim();
        if let Ok(n) = text.parse::<i64>() {
            return Some(n);
        }
        if crate::environment::is_valid_identifier(text) {
            return self.get_var(text).and_then(|v| v.trim().parse().ok());
        }
        None
    }

    /// Element `index` of indexed array `name` (negative counts from the end),
    /// or key `index` of an associative array. A scalar is a one-element
    /// array. `index` is an already evaluated integer for indexed arrays.
    pub fn get_array_element(&self, name: &str, index: &str) -> Option<String> {
        let target = self.resolve_nameref(name).ok()?;
        let var = self.find_variable(&target.name)?;
        match &var.value {
            VarValue::Assoc(m) => m.get(index).map(str::to_string),
            VarValue::Indexed(a) => {
                let i = self.literal_index(index)?;
                a.get(a.resolve(i)?).map(str::to_string)
            }
            VarValue::Scalar(s) => (self.literal_index(index)? == 0).then(|| s.clone()),
            VarValue::Unset => None,
        }
    }

    /// Element `index` of an indexed array, negative counting from the end.
    pub fn indexed_element(&self, name: &str, index: i64) -> Option<&str> {
        let target = self.resolve_nameref(name).ok()?;
        match &self.find_variable(&target.name)?.value {
            VarValue::Indexed(a) => a.get(a.resolve(index)?),
            VarValue::Scalar(s) if index == 0 || index == -1 => Some(s),
            VarValue::Assoc(m) => m.get(&index.to_string()),
            _ => None,
        }
    }

    /// The values of array `name` in index order (bash's order for an
    /// associative array). Empty for scalars and unset names.
    pub fn array_values(&self, name: &str) -> Vec<String> {
        match self.resolved_variable(name).map(|v| &v.value) {
            Some(VarValue::Indexed(a)) => a.values().cloned().collect(),
            Some(VarValue::Assoc(m)) => m.values(),
            _ => Vec::new(),
        }
    }

    /// The subscripts of array `name`: set indices, or associative keys.
    pub fn array_keys(&self, name: &str) -> Vec<String> {
        match self.resolved_variable(name).map(|v| &v.value) {
            Some(VarValue::Indexed(a)) => a.indices().map(|i| i.to_string()).collect(),
            Some(VarValue::Assoc(m)) => m.keys(),
            _ => Vec::new(),
        }
    }

    /// The number of set elements of array `name`.
    pub fn array_length(&self, name: &str) -> usize {
        match self.resolved_variable(name).map(|v| &v.value) {
            Some(VarValue::Indexed(a)) => a.len(),
            Some(VarValue::Assoc(m)) => m.len(),
            _ => 0,
        }
    }

    /// Is `name` an indexed or associative array (possibly declared only)?
    pub fn is_array(&self, name: &str) -> bool {
        self.resolved_variable(name).is_some_and(Variable::is_array)
    }

    pub fn is_assoc(&self, name: &str) -> bool {
        self.resolved_variable(name).is_some_and(Variable::is_assoc)
    }

    /// Bash's `-v name` test: is the variable (or `name[subscript]`) set?
    pub fn is_var_set(&self, name: &str) -> bool {
        if let Some((base, subscript)) = split_subscripted(name) {
            return match subscript {
                "@" | "*" => self.array_length(base) > 0,
                _ => self.get_array_element(base, subscript).is_some(),
            };
        }
        if self.is_dynamic_var(name) {
            return true;
        }
        match self.resolved_variable(name) {
            // An array counts as set when element 0 is.
            Some(var) => var.scalar_value().is_some(),
            None => false,
        }
    }

    /// Names of every visible variable that is set or declared, sorted.
    pub fn var_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.globals.keys().cloned().collect();
        for scope in &self.local_vars_stack {
            names.extend(scope.vars.keys().cloned());
        }
        names.sort();
        names.dedup();
        names
    }

    /// Every visible variable with the scope it is visible from, sorted by
    /// name. Unset declarations are included; callers filter as they need.
    pub fn visible_variables(&self) -> Vec<(&str, Slot, &Variable)> {
        self.var_names_ref()
            .into_iter()
            .filter_map(|name| {
                let slot = self.slot_of(name)?;
                Some((name, slot, self.var_at(slot, name)?))
            })
            .collect()
    }

    fn var_names_ref(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.globals.keys().map(String::as_str).collect();
        for scope in &self.local_vars_stack {
            names.extend(scope.vars.keys().map(String::as_str));
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    // ------------------------------------------------------------------
    // The environment
    // ------------------------------------------------------------------

    /// The value `name` has in the environment of executed commands: the
    /// innermost variable of that name that is set and exported. As in bash,
    /// a declared-but-unset or unexported variable does not hide an exported
    /// one further out, and arrays are never exported.
    pub fn exported_value(&self, name: &str) -> Option<&str> {
        let candidates = self
            .local_vars_stack
            .iter()
            .rev()
            .filter_map(|scope| scope.vars.get(name))
            .chain(self.globals.get(name));
        for var in candidates {
            if var.attrs.contains(VarAttrs::EXPORT) && var.is_set() {
                return match &var.value {
                    VarValue::Scalar(s) => Some(s),
                    _ => None,
                };
            }
        }
        None
    }

    /// Every exported `(name, value)`, sorted by name.
    pub fn exported_vars(&self) -> Vec<(String, String)> {
        self.var_names_ref()
            .into_iter()
            .filter_map(|name| Some((name.to_string(), self.exported_value(name)?.to_string())))
            .collect()
    }

    /// Bring the process environment entry for `name` in line with the
    /// shell's exported view of it. External commands inherit the process
    /// environment through `execvp`, so it has to be exact.
    pub(crate) fn sync_env(&self, name: &str) {
        match self.exported_value(name) {
            Some(value) => {
                if std::env::var_os(name).as_deref() != Some(std::ffi::OsStr::new(value)) {
                    set_env_checked(name, value);
                    note_env_name(name);
                }
            }
            None => {
                // Every assignment to an unexported variable lands here, and
                // getenv locks and scans the whole environment: skip it for
                // names the environment has never held.
                if may_be_in_env(name) && std::env::var_os(name).is_some() {
                    remove_env_checked(name);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Assignment
    // ------------------------------------------------------------------

    /// Report a refused assignment the way bash does — immediately, on
    /// stderr — and remember it for [`Self::take_assignment_failure`].
    pub(crate) fn report_assignment_error(&mut self, message: &str) {
        eprintln!("jsh: {message}");
        self.assignment_failed = true;
        self.assignment_failure_fatal = false;
    }

    /// Was an assignment refused since the flag was last taken? Callers clear
    /// it before an operation and check it after: an assignment statement
    /// turns it into a fatal error for the command line, a builtin into
    /// status 1.
    pub fn take_assignment_failure(&mut self) -> bool {
        std::mem::take(&mut self.assignment_failed)
    }

    fn allexport(&self) -> bool {
        self.shell_opts
            .tracked_opts
            .get("allexport")
            .copied()
            .unwrap_or(false)
    }

    /// Evaluate an integer variable's assignment. `None` when the expression
    /// is malformed; the error has been reported.
    fn eval_integer(&mut self, expr: &str) -> Option<i64> {
        let outer = self.expansion_error.take();
        let text = crate::expand::expand_arithmetic(expr, self);
        match self.expansion_error.take() {
            Some(error) => {
                self.expansion_error = outer;
                self.report_assignment_error(&error);
                // A malformed `-i` value ends a bash script.
                self.assignment_failure_fatal = true;
                None
            }
            None => {
                self.expansion_error = outer;
                Some(text.trim().parse().unwrap_or(0))
            }
        }
    }

    /// The value actually stored when `value` is assigned to a variable with
    /// `attrs`: arithmetic for `-i`, case conversion for `-l`/`-u`/`-c`.
    /// `old` is the current value for `+=`. `None` if evaluation failed.
    fn transform_value(
        &mut self,
        attrs: VarAttrs,
        value: &str,
        old: Option<&str>,
    ) -> Option<String> {
        if attrs.contains(VarAttrs::INTEGER) {
            let mut n = self.eval_integer(value)?;
            if let Some(old) = old {
                let base = self.eval_integer(if old.is_empty() { "0" } else { old })?;
                n = base.wrapping_add(n);
            }
            return Some(n.to_string());
        }
        let joined = match old {
            Some(old) => format!("{old}{value}"),
            None => value.to_string(),
        };
        Some(if attrs.intersects(VarAttrs::CASE) {
            apply_case(attrs, &joined)
        } else {
            joined
        })
    }

    /// Resolve an assignment target, reporting a circular nameref.
    fn assignment_target(&mut self, name: &str) -> Option<VarRef> {
        match self.resolve_nameref(name) {
            Ok(target) => Some(target),
            Err(message) => {
                self.report_assignment_error(&message);
                None
            }
        }
    }

    /// Refuse to modify a readonly variable, reporting it as bash does.
    fn check_writable(&mut self, slot: Option<Slot>, name: &str) -> bool {
        let readonly = slot
            .and_then(|slot| self.var_at(slot, name))
            .is_some_and(|v| v.attrs.contains(VarAttrs::READONLY));
        if readonly {
            self.report_assignment_error(&format!("{name}: readonly variable"));
        }
        !readonly
    }

    /// Assign `$name` (an ordinary `name=value`): the innermost scope that
    /// holds `name`, else a new global. An array gets element 0.
    pub fn set_var(&mut self, name: &str, value: &str) {
        self.assign_scalar(name, value, false);
    }

    /// `name+=value` for a scalar: string append, or integer addition for a
    /// `-i` variable. An array appends to element 0.
    pub fn append_var(&mut self, name: &str, value: &str) {
        self.assign_scalar(name, value, true);
    }

    /// Assign or append to `$name`; false when the assignment was refused
    /// (and reported).
    pub fn assign_scalar(&mut self, name: &str, value: &str, append: bool) -> bool {
        let Some(target) = self.assignment_target(name) else {
            return false;
        };
        if let Some(subscript) = target.subscript {
            return self.assign_element(&target.name, &subscript, value, append);
        }
        let name = target.name;
        match self.slot_of(&name) {
            Some(slot) => self.assign_scalar_at(slot, &name, value, append),
            None => {
                let Some(stored) =
                    self.transform_value(VarAttrs::NONE, value, append.then_some(""))
                else {
                    return false;
                };
                // Evaluating the value cannot have declared `name` in an inner
                // scope, but it may have created it as a global.
                let slot = self.slot_of(&name).unwrap_or(Slot::Global);
                self.store_scalar_at(slot, &name, stored);
                true
            }
        }
    }

    /// Assign or append to the variable `name` in exactly `slot` (the
    /// declaration builtins pick the scope themselves), applying its
    /// attributes. False when refused (and reported).
    pub fn assign_scalar_at(&mut self, slot: Slot, name: &str, value: &str, append: bool) -> bool {
        if !self.check_writable(Some(slot), name) {
            return false;
        }
        let (attrs, old) = match self.var_at(slot, name) {
            Some(var) => (var.attrs, var.scalar_value().map(str::to_string)),
            None => (VarAttrs::NONE, None),
        };
        let old = if append {
            Some(old.unwrap_or_default())
        } else {
            None
        };
        let Some(stored) = self.transform_value(attrs, value, old.as_deref()) else {
            return false;
        };
        self.store_scalar_at(slot, name, stored);
        true
    }

    /// Store an already transformed value into `name` at `slot` (no attribute
    /// processing), creating the variable there if needed.
    fn store_scalar_at(&mut self, slot: Slot, name: &str, value: String) {
        let allexport = self.allexport();
        let var = self.table_mut(slot).entry(name.to_string()).or_default();
        match &mut var.value {
            VarValue::Indexed(a) => a.set(0, value.clone()),
            VarValue::Assoc(m) => m.insert("0".to_string(), value.clone()),
            other => {
                if var.attrs.contains(VarAttrs::ARRAY) {
                    let mut a = IndexedArray::new();
                    a.set(0, value.clone());
                    *other = VarValue::Indexed(a);
                } else if var.attrs.contains(VarAttrs::ASSOC) {
                    let mut m = AssocArray::new();
                    m.insert("0".to_string(), value.clone());
                    *other = VarValue::Assoc(m);
                } else {
                    *other = VarValue::Scalar(value.clone());
                }
            }
        }
        if allexport {
            var.attrs.insert(VarAttrs::EXPORT);
        }
        self.after_assign(name, slot, &value);
    }

    /// Side effects of assigning `name`: special variables and the
    /// environment.
    fn after_assign(&mut self, name: &str, slot: Slot, value: &str) {
        if slot == Slot::Global && self.dynamic.is_active(name) {
            match name {
                "RANDOM" => {
                    if let Ok(seed) = value.trim().parse::<i64>() {
                        self.dynamic.seed_random(seed as u32);
                    }
                }
                "SECONDS" => {
                    self.dynamic.seconds_assigned = value.trim().parse().unwrap_or(0);
                    self.dynamic.seconds_base = now_epoch().as_secs() as i64;
                }
                _ => {}
            }
        }
        if name == "PATH" {
            self.invalidate_path_cache();
        }
        self.sync_env(name);
    }

    /// `export name=value`: assign, then give the variable the export
    /// attribute (in whichever scope the assignment landed).
    pub fn export_var(&mut self, name: &str, value: &str) {
        if self.assign_scalar(name, value, false) {
            self.set_attrs(name, VarAttrs::EXPORT, VarAttrs::NONE);
        }
    }

    /// Turn attributes on and off for the variable `name` resolves to,
    /// creating an unset global declaration when none exists (`export X`).
    pub fn set_attrs(&mut self, name: &str, on: VarAttrs, off: VarAttrs) {
        let target = self
            .resolve_nameref(name)
            .map(|t| t.name)
            .unwrap_or_else(|_| name.to_string());
        let slot = self.slot_of(&target).unwrap_or(Slot::Global);
        let var = self.table_mut(slot).entry(target.clone()).or_default();
        var.attrs.insert(on);
        var.attrs.remove(off);
        self.sync_env(&target);
    }

    /// Unset `name` as the `unset` builtin does, following namerefs:
    /// a local of the running function stays declared (and keeps shadowing
    /// outer variables) but loses its value and attributes; anything else is
    /// removed from its scope, revealing whatever it shadowed. Readonly
    /// variables are refused.
    pub fn unset_variable(&mut self, name: &str) -> Result<(), String> {
        let target = self.resolve_nameref(name)?;
        if let Some(subscript) = target.subscript {
            return self.unset_element(&target.name, &subscript);
        }
        self.unset_exact(&target.name)
    }

    /// `unset -n name`: unset the variable itself even when it is a nameref.
    pub fn unset_exact(&mut self, name: &str) -> Result<(), String> {
        let Some(slot) = self.slot_of(name) else {
            self.disable_dynamic(name);
            return Ok(());
        };
        if self
            .var_at(slot, name)
            .is_some_and(|v| v.attrs.contains(VarAttrs::READONLY))
        {
            return Err(format!("{name}: cannot unset: readonly variable"));
        }
        let own_scope = matches!(slot, Slot::Local(i) if Some(i) == self.function_scope());
        if own_scope {
            if let Some(var) = self.var_at_mut(slot, name) {
                *var = Variable::default();
            }
        } else {
            self.table_mut(slot).remove(name);
        }
        if slot == Slot::Global {
            self.disable_dynamic(name);
        }
        if name == "PATH" {
            self.invalidate_path_cache();
        }
        self.sync_env(name);
        Ok(())
    }

    /// `unset RANDOM` (and the other computed variables) removes the special
    /// meaning for good, as in bash.
    fn disable_dynamic(&mut self, name: &str) {
        if let Some(dynamic) = DYNAMIC_NAMES.iter().find(|d| **d == name) {
            if !self.dynamic.disabled.contains(dynamic) {
                self.dynamic.disabled.push(dynamic);
            }
        }
    }

    /// Unset `name` (see [`Self::unset_variable`]); a readonly variable is
    /// silently kept. For internal callers that are not the `unset` builtin.
    pub fn unset_var(&mut self, name: &str) {
        let _ = self.unset_variable(name);
    }

    // ------------------------------------------------------------------
    // Arrays
    // ------------------------------------------------------------------

    /// Evaluate an indexed-array subscript. The subscript is an arithmetic
    /// expression (`a[i+1]`); a plain integer skips the evaluator.
    pub(crate) fn eval_index(&mut self, subscript: &str) -> Option<i64> {
        if let Ok(n) = subscript.trim().parse::<i64>() {
            return Some(n);
        }
        self.eval_integer(subscript)
    }

    /// `name[subscript]=value` (or `+=`): an associative array takes the
    /// subscript as its key; anything else treats it as an arithmetic index,
    /// negative counting back from the end. A scalar becomes an array whose
    /// element 0 is the old value. False when refused (and reported).
    pub fn assign_element(
        &mut self,
        name: &str,
        subscript: &str,
        value: &str,
        append: bool,
    ) -> bool {
        let Some(target) = self.assignment_target(name) else {
            return false;
        };
        let name = target.name;
        let slot = self.slot_of(&name);
        if !self.check_writable(slot, &name) {
            return false;
        }
        let (attrs, assoc) = match slot.and_then(|s| self.var_at(s, &name)) {
            Some(var) => (var.attrs, var.is_assoc()),
            None => (VarAttrs::NONE, false),
        };
        if assoc {
            let old = append.then(|| self.assoc_get(&name, subscript).unwrap_or_default());
            let Some(stored) = self.transform_value(attrs, value, old.as_deref()) else {
                return false;
            };
            let slot = self.slot_of(&name).unwrap_or(Slot::Global);
            let var = self.table_mut(slot).entry(name.clone()).or_default();
            if !matches!(var.value, VarValue::Assoc(_)) {
                var.value = VarValue::Assoc(AssocArray::new());
            }
            if let VarValue::Assoc(m) = &mut var.value {
                m.insert(subscript.to_string(), stored);
            }
            self.sync_env(&name);
            return true;
        }
        let Some(index) = self.eval_index(subscript) else {
            return false;
        };
        let resolved = {
            let array = self.find_variable(&name).map(|v| &v.value);
            match array {
                Some(VarValue::Indexed(a)) => a.resolve(index),
                Some(VarValue::Scalar(_)) => {
                    if index >= 0 {
                        Some(index)
                    } else {
                        // A scalar is a one-element array: -1 is element 0.
                        (index == -1).then_some(0)
                    }
                }
                _ => (index >= 0).then_some(index),
            }
        };
        let Some(index) = resolved else {
            self.report_assignment_error(&format!("{name}[{subscript}]: bad array subscript"));
            return false;
        };
        let old = if append {
            Some(self.indexed_element(&name, index).unwrap_or("").to_string())
        } else {
            None
        };
        let Some(stored) = self.transform_value(attrs, value, old.as_deref()) else {
            return false;
        };
        let slot = self.slot_of(&name).unwrap_or(Slot::Global);
        let allexport = self.allexport();
        let var = self.table_mut(slot).entry(name.clone()).or_default();
        let mut array = match std::mem::take(&mut var.value) {
            VarValue::Indexed(a) => a,
            VarValue::Scalar(s) => IndexedArray::from_values(vec![s]),
            _ => IndexedArray::new(),
        };
        array.set(index, stored);
        var.value = VarValue::Indexed(array);
        if allexport {
            var.attrs.insert(VarAttrs::EXPORT);
        }
        self.sync_env(&name);
        true
    }

    fn assoc_get(&self, name: &str, key: &str) -> Option<String> {
        match &self.find_variable(name)?.value {
            VarValue::Assoc(m) => m.get(key).map(str::to_string),
            _ => None,
        }
    }

    /// Set one element of array `name`; `index` is an associative key or an
    /// arithmetic index expression (see [`Self::assign_element`]).
    pub fn set_array_element(&mut self, name: &str, index: &str, value: &str) {
        self.assign_element(name, index, value, false);
    }

    /// Replace the array `name` resolves to (a visible local, else the global)
    /// with the dense list `values`. Used for `read -a`, `mapfile`,
    /// `BASH_REMATCH`, `PIPESTATUS` and the like.
    pub fn set_array(&mut self, name: &str, values: Vec<String>) {
        let elements = values
            .into_iter()
            .map(|value| CompoundElement {
                key: None,
                value,
                append: false,
            })
            .collect();
        let target = self
            .resolve_nameref(name)
            .map(|t| t.name)
            .unwrap_or_else(|_| name.to_string());
        let slot = self.slot_of(&target);
        // An associative array is replaced by an indexed one: these callers
        // produce lists.
        if let Some(var) = slot.and_then(|s| self.var_at_mut(s, &target)) {
            if var.is_assoc() && !var.attrs.contains(VarAttrs::READONLY) {
                var.attrs.remove(VarAttrs::ASSOC);
                var.value = VarValue::Unset;
            }
        }
        let _ = self.assign_compound(&target, slot, elements, false, Some(ArrayKind::Indexed));
    }

    /// Replace one of the arrays the shell maintains for itself on every
    /// function call and return or after every pipeline (`FUNCNAME`,
    /// `BASH_SOURCE`, `BASH_LINENO`, `PIPESTATUS`) in the global scope.
    ///
    /// This skips the general assignment path. These names are rewritten
    /// twice per call, and going through [`Self::set_array`] (nameref
    /// resolution, compound assignment, then a process-environment check per
    /// array) made function calls about three times slower. An array is never
    /// exported, so the environment only needs attention when the name was
    /// inherited from it as a scalar.
    pub(crate) fn set_shell_array(&mut self, name: &str, values: Vec<String>) {
        let value = VarValue::Indexed(IndexedArray::from_values(values));
        let var = self.globals.entry(name.to_string()).or_default();
        if var.attrs.contains(VarAttrs::EXPORT) {
            var.attrs.remove(VarAttrs::EXPORT);
            remove_env_checked(name);
        }
        var.value = value;
    }

    /// Drop a shell-maintained array set by [`Self::set_shell_array`].
    pub(crate) fn unset_shell_array(&mut self, name: &str) {
        self.globals.remove(name);
    }

    /// Append `values` to indexed array `name` (`arr+=(...)` with plain words).
    pub fn append_array(&mut self, name: &str, values: Vec<String>) {
        let elements = values
            .into_iter()
            .map(|value| CompoundElement {
                key: None,
                value,
                append: false,
            })
            .collect();
        let slot = self.slot_of(name);
        let _ = self.assign_compound(name, slot, elements, true, None);
    }

    /// Perform a compound assignment `name=(...)` / `name+=(...)` into the
    /// variable at `slot` (`None`: create a global). `kind` forces the array
    /// kind (`declare -a`/`-A`); otherwise an existing associative array stays
    /// associative and anything else becomes indexed. Element values go
    /// through the variable's attributes. On error nothing is changed.
    pub fn assign_compound(
        &mut self,
        name: &str,
        slot: Option<Slot>,
        elements: Vec<CompoundElement>,
        append: bool,
        kind: Option<ArrayKind>,
    ) -> Result<(), String> {
        let slot = slot.filter(|s| self.var_at(*s, name).is_some());
        let (attrs, existing_assoc) = match slot.and_then(|s| self.var_at(s, name)) {
            Some(var) => (var.attrs, var.is_assoc()),
            None => (VarAttrs::NONE, false),
        };
        if attrs.contains(VarAttrs::READONLY) {
            let message = format!("{name}: readonly variable");
            self.report_assignment_error(&message);
            return Err(message);
        }
        let kind = kind.unwrap_or(if existing_assoc {
            ArrayKind::Assoc
        } else {
            ArrayKind::Indexed
        });
        // An append extends the array where it is; take it out of the
        // variable for the duration (cloning it made every `arr+=(x)` in a
        // loop copy the whole array) and put the original back on error.
        let original = match (append, slot) {
            (true, Some(slot)) => self
                .var_at_mut(slot, name)
                .map(|var| std::mem::take(&mut var.value))
                .unwrap_or_default(),
            _ => VarValue::Unset,
        };
        let restore = |state: &mut ShellState, value: VarValue| {
            if let Some(var) = slot.and_then(|slot| state.var_at_mut(slot, name)) {
                var.value = value;
            }
        };
        let new_value = match kind {
            ArrayKind::Indexed => {
                let array = match original {
                    VarValue::Indexed(a) => a,
                    VarValue::Scalar(s) => IndexedArray::from_values(vec![s]),
                    _ => IndexedArray::new(),
                };
                match self.fill_indexed(name, attrs, array, elements) {
                    Ok(array) => VarValue::Indexed(array),
                    Err((message, array)) => {
                        let value = if array.is_empty() && !append {
                            VarValue::Unset
                        } else {
                            VarValue::Indexed(array)
                        };
                        if append {
                            restore(self, value);
                        }
                        return Err(message);
                    }
                }
            }
            ArrayKind::Assoc => {
                let map = match original {
                    VarValue::Assoc(m) => m,
                    other => {
                        if append {
                            restore(self, other);
                        }
                        AssocArray::new()
                    }
                };
                match self.fill_assoc(name, attrs, map, elements) {
                    Ok(map) => VarValue::Assoc(map),
                    Err((message, map)) => {
                        if append {
                            restore(self, VarValue::Assoc(map));
                        }
                        return Err(message);
                    }
                }
            }
        };
        let slot = slot.or_else(|| self.slot_of(name)).unwrap_or(Slot::Global);
        let allexport = self.allexport();
        let var = self.table_mut(slot).entry(name.to_string()).or_default();
        match new_value {
            VarValue::Indexed(_) => {
                var.attrs.remove(VarAttrs::ASSOC);
                var.attrs.insert(VarAttrs::ARRAY);
            }
            VarValue::Assoc(_) => {
                var.attrs.remove(VarAttrs::ARRAY);
                var.attrs.insert(VarAttrs::ASSOC);
            }
            _ => {}
        }
        var.value = new_value;
        if allexport {
            var.attrs.insert(VarAttrs::EXPORT);
        }
        self.sync_env(name);
        Ok(())
    }

    /// Apply compound-assignment elements to an indexed array. On error the
    /// array comes back as it was, with the message.
    fn fill_indexed(
        &mut self,
        name: &str,
        attrs: VarAttrs,
        mut array: IndexedArray,
        elements: Vec<CompoundElement>,
    ) -> Result<IndexedArray, (String, IndexedArray)> {
        // What each assignment replaced, newest last, to undo a failure.
        let mut undo: Vec<(i64, Option<String>)> = Vec::new();
        let mut next = array.next_index();
        let mut failure = None;
        for element in elements {
            let index = match &element.key {
                Some(key) => match self.eval_index(key) {
                    Some(raw) => array
                        .resolve(raw)
                        .ok_or_else(|| format!("{name}[{key}]: bad array subscript")),
                    // The arithmetic error has been reported.
                    None => {
                        failure = Some(format!("{name}: bad array subscript"));
                        break;
                    }
                },
                None => next.ok_or_else(|| format!("{name}: bad array subscript")),
            };
            let index = match index {
                Ok(index) => index,
                Err(message) => {
                    self.report_assignment_error(&message);
                    failure = Some(message);
                    break;
                }
            };
            let old = element
                .append
                .then(|| array.get(index).unwrap_or("").to_string());
            let Some(stored) = self.transform_value(attrs, &element.value, old.as_deref()) else {
                failure = Some(format!("{name}: assignment failed"));
                break;
            };
            undo.push((index, array.get(index).map(str::to_string)));
            array.set(index, stored);
            next = index.checked_add(1);
        }
        match failure {
            None => Ok(array),
            Some(message) => {
                for (index, old) in undo.into_iter().rev() {
                    match old {
                        Some(value) => array.set(index, value),
                        None => {
                            array.remove(index);
                        }
                    }
                }
                Err((message, array))
            }
        }
    }

    /// Apply compound-assignment elements to an associative array; see
    /// [`Self::fill_indexed`].
    fn fill_assoc(
        &mut self,
        name: &str,
        attrs: VarAttrs,
        mut map: AssocArray,
        elements: Vec<CompoundElement>,
    ) -> Result<AssocArray, (String, AssocArray)> {
        // `m=(k1 v1 k2 v2)` with no subscripts at all is a list of key/value
        // pairs (bash 5.1+).
        let pairs: Vec<(String, String, bool)> =
            if !elements.is_empty() && elements.iter().all(|e| e.key.is_none()) {
                let mut iter = elements.into_iter();
                let mut pairs = Vec::new();
                while let Some(key) = iter.next() {
                    let value = iter.next().map(|e| e.value).unwrap_or_default();
                    pairs.push((key.value, value, false));
                }
                pairs
            } else {
                elements
                    .into_iter()
                    .filter_map(|element| match element.key {
                        Some(key) => Some((key, element.value, element.append)),
                        None => {
                            eprintln!(
                            "jsh: {name}: {}: must use subscript when assigning associative array",
                            element.value
                        );
                            None
                        }
                    })
                    .collect()
            };
        let mut undo: Vec<(String, Option<String>)> = Vec::new();
        for (key, value, append) in pairs {
            let old = append.then(|| map.get(&key).unwrap_or("").to_string());
            let Some(stored) = self.transform_value(attrs, &value, old.as_deref()) else {
                for (key, old) in undo.into_iter().rev() {
                    match old {
                        Some(value) => map.insert(key, value),
                        None => {
                            map.remove(&key);
                        }
                    }
                }
                return Err((format!("{name}: assignment failed"), map));
            };
            undo.push((key.clone(), map.get(&key).map(str::to_string)));
            map.insert(key, stored);
        }
        Ok(map)
    }

    /// `name=(...)` / `name+=(...)` from an assignment statement: the
    /// variable `name` resolves to (a visible local, else a global) becomes
    /// an array. A refused assignment is reported.
    pub fn assign_array_literal(
        &mut self,
        name: &str,
        elements: Vec<CompoundElement>,
        append: bool,
    ) {
        let Some(target) = self.assignment_target(name) else {
            return;
        };
        let slot = self.slot_of(&target.name);
        let _ = self.assign_compound(&target.name, slot, elements, append, None);
    }

    /// Set a `for`/`select` loop variable. A nameref loop variable is
    /// pointed at each word in turn (bash's special case) instead of
    /// assigning through it.
    pub fn assign_loop_variable(&mut self, name: &str, value: &str) {
        if let Some(slot) = self.slot_of(name) {
            if self
                .var_at(slot, name)
                .is_some_and(|v| v.attrs.contains(VarAttrs::NAMEREF))
            {
                if !self.check_writable(Some(slot), name) {
                    return;
                }
                if let Some(var) = self.var_at_mut(slot, name) {
                    var.value = VarValue::Scalar(value.to_string());
                }
                return;
            }
        }
        self.set_var(name, value);
    }

    /// `unset 'name[subscript]'`.
    pub fn unset_element(&mut self, name: &str, subscript: &str) -> Result<(), String> {
        let Some(slot) = self.slot_of(name) else {
            return Ok(());
        };
        let Some(var) = self.var_at(slot, name) else {
            return Ok(());
        };
        if var.attrs.contains(VarAttrs::READONLY) {
            return Err(format!("{name}: cannot unset: readonly variable"));
        }
        match &var.value {
            VarValue::Assoc(_) => {
                if let Some(VarValue::Assoc(m)) = self.var_at_mut(slot, name).map(|v| &mut v.value)
                {
                    m.remove(subscript);
                }
            }
            VarValue::Indexed(_) | VarValue::Scalar(_) => {
                if subscript == "@" || subscript == "*" {
                    return self.unset_exact(name);
                }
                let Some(raw) = self.eval_index(subscript) else {
                    return Ok(());
                };
                let var = self.var_at_mut(slot, name).expect("located above");
                match &mut var.value {
                    VarValue::Indexed(a) => match a.resolve(raw) {
                        Some(i) => {
                            a.remove(i);
                        }
                        None => return Err(format!("{name}[{subscript}]: bad array subscript")),
                    },
                    VarValue::Scalar(_) if raw == 0 || raw == -1 => {
                        return self.unset_exact(name);
                    }
                    _ => {}
                }
            }
            VarValue::Unset => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Scopes
    // ------------------------------------------------------------------

    /// Push a new local variable scope (for function entry).
    pub fn push_local_scope(&mut self) {
        self.local_vars_stack.push(VarScope::default());
    }

    /// Push a scope for a command's prefix assignments (`NAME=value cmd`).
    pub fn push_temp_scope(&mut self) {
        self.local_vars_stack.push(VarScope {
            vars: HashMap::new(),
            temporary: true,
        });
    }

    /// Bind `name=value` in the innermost (temporary) scope, exported, as a
    /// prefix assignment does for the command it precedes. A readonly
    /// variable is refused and reported.
    pub fn set_temp_var(&mut self, name: &str, value: &str) -> bool {
        let slot = self.slot_of(name);
        if !self.check_writable(slot, name) {
            return false;
        }
        let attrs = slot
            .and_then(|s| self.var_at(s, name))
            .map(|v| v.attrs)
            .unwrap_or_default();
        let Some(stored) = self.transform_value(attrs, value, None) else {
            return false;
        };
        let Some(scope) = self.local_vars_stack.last_mut() else {
            self.store_scalar_at(Slot::Global, name, stored);
            return true;
        };
        let mut attrs = attrs;
        attrs.remove(
            VarAttrs::ARRAY
                .union(VarAttrs::ASSOC)
                .union(VarAttrs::NAMEREF),
        );
        attrs.insert(VarAttrs::EXPORT);
        scope.vars.insert(
            name.to_string(),
            Variable {
                value: VarValue::Scalar(stored),
                attrs,
            },
        );
        self.sync_env(name);
        true
    }

    /// Pop the current local variable scope (for function exit), restoring
    /// the environment entries its exported variables had shadowed.
    pub fn pop_local_scope(&mut self) {
        if let Some(scope) = self.local_vars_stack.pop() {
            for name in scope.vars.keys() {
                self.sync_env(name);
            }
        }
    }

    /// Create (or find) `name` in the running function's scope, as `local`
    /// and `declare` inside a function do. A new local starts unset, except
    /// that it inherits the export attribute of the variable it shadows and
    /// the value of a temporary-environment variable (`x=4 f` then `local x`
    /// in `f`), both as in bash. Making a local copy of a readonly global is
    /// refused.
    pub fn declare_local(&mut self, name: &str) -> Result<Slot, String> {
        let Some(scope) = self.function_scope() else {
            return Err("can only be used in a function".to_string());
        };
        if self.local_vars_stack[scope].vars.contains_key(name) {
            return Ok(Slot::Local(scope));
        }
        let mut var = Variable::default();
        match self.slot_of(name) {
            Some(Slot::Global) => {
                let outer = &self.globals[name];
                if outer.attrs.contains(VarAttrs::READONLY) {
                    return Err(format!("{name}: readonly variable"));
                }
                if outer.attrs.contains(VarAttrs::EXPORT) {
                    var.attrs.insert(VarAttrs::EXPORT);
                }
            }
            Some(Slot::Local(i)) => {
                let outer = &self.local_vars_stack[i];
                let outer_var = &outer.vars[name];
                if outer_var.attrs.contains(VarAttrs::EXPORT) {
                    var.attrs.insert(VarAttrs::EXPORT);
                }
                if outer.temporary {
                    if let VarValue::Scalar(s) = &outer_var.value {
                        var.value = VarValue::Scalar(s.clone());
                    }
                }
            }
            None => {}
        }
        self.local_vars_stack[scope]
            .vars
            .insert(name.to_string(), var);
        Ok(Slot::Local(scope))
    }

    /// The slot `declare -g name` operates on, creating an unset global
    /// declaration when there is none.
    pub fn declare_global(&mut self, name: &str) -> Slot {
        self.globals.entry(name.to_string()).or_default();
        Slot::Global
    }

    /// Increment `SHLVL` the way a starting bash does: an unset or
    /// non-numeric value counts as 0, a negative result becomes 0, a level of
    /// 1000 or more is reset to 1 with a warning, and the result is exported.
    pub fn increment_shlvl(&mut self) {
        let mut level = self
            .get_var("SHLVL")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        if level < 0 {
            level = 0;
        } else if level >= 1000 {
            eprintln!("jsh: warning: shell level ({level}) too high, resetting to 1");
            level = 1;
        }
        self.export_var("SHLVL", &level.to_string());
    }

    /// Install the variables bash defines at startup: `UID`, `EUID` and
    /// `PPID` (readonly integers), `PWD` and `OLDPWD`, and `HOSTNAME`,
    /// `HOSTTYPE`, `OSTYPE`, `MACHTYPE` unless the environment already
    /// supplied them.
    pub(crate) fn init_shell_variables(&mut self) {
        // SAFETY: these libc calls take no arguments and cannot fail.
        let (uid, euid, ppid) = unsafe {
            (
                nix::libc::getuid(),
                nix::libc::geteuid(),
                nix::libc::getppid(),
            )
        };
        let readonly_int = VarAttrs::READONLY.union(VarAttrs::INTEGER);
        for (name, value) in [("UID", uid.to_string()), ("EUID", euid.to_string())] {
            if !self.globals.contains_key(name) {
                self.globals.insert(
                    name.to_string(),
                    Variable {
                        value: VarValue::Scalar(value),
                        attrs: readonly_int,
                    },
                );
            }
        }
        // PPID always describes this process's parent, whatever was inherited.
        let ppid_attrs = self
            .globals
            .get("PPID")
            .map_or(VarAttrs::NONE, |v| v.attrs)
            .union(readonly_int);
        self.globals.insert(
            "PPID".to_string(),
            Variable {
                value: VarValue::Scalar(ppid.to_string()),
                attrs: ppid_attrs,
            },
        );
        self.sync_env("PPID");
        let arch = std::env::consts::ARCH;
        let ostype = if cfg!(target_os = "linux") {
            if cfg!(target_env = "musl") {
                "linux-musl"
            } else {
                "linux-gnu"
            }
        } else {
            std::env::consts::OS
        };
        let vendor = if matches!(arch, "x86_64" | "x86") {
            "pc"
        } else {
            "unknown"
        };
        let defaults = [
            (
                "HOSTNAME",
                system_hostname().unwrap_or_else(|| self.hostname.clone()),
            ),
            ("HOSTTYPE", arch.to_string()),
            ("OSTYPE", ostype.to_string()),
            ("MACHTYPE", format!("{arch}-{vendor}-{ostype}")),
        ];
        // PWD is exported and must name the working directory; an inherited
        // value that names some other directory is replaced. OLDPWD starts
        // out exported but unset.
        if let Ok(cwd) = std::env::current_dir() {
            let inherited_ok = self
                .globals
                .get("PWD")
                .and_then(Variable::scalar_value)
                .is_some_and(|pwd| pwd.starts_with('/') && same_directory(pwd, &cwd));
            if !inherited_ok {
                self.globals.insert(
                    "PWD".to_string(),
                    Variable::exported(cwd.to_string_lossy().into_owned()),
                );
                self.sync_env("PWD");
            }
        }
        self.globals
            .entry("OLDPWD".to_string())
            .or_insert_with(|| Variable {
                value: VarValue::Unset,
                attrs: VarAttrs::EXPORT,
            });
        for (name, value) in defaults {
            self.globals
                .entry(name.to_string())
                .or_insert_with(|| Variable::scalar(value));
        }
    }
}

// ----------------------------------------------------------------------
// Quoting, as `declare -p`, `${v@Q}` and `${v@A}` print values
// ----------------------------------------------------------------------

/// Does bash's `ansic_shouldquote` want `$'...'` for this text? True when it
/// holds a character that is not printable.
fn needs_ansi_c_quoting(text: &str) -> bool {
    text.chars().any(|c| c.is_control())
}

/// Bash's `ansic_quote`: `$'...'` with C escapes.
pub fn ansi_c_quote(text: &str) -> String {
    let mut out = String::from("$'");
    for c in text.chars() {
        match c {
            '\x1b' => out.push_str("\\E"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\{:03o}", byte));
                }
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A value as `declare -p` prints it: `"..."` with `"`, `$`, `` ` `` and `\`
/// escaped, or `$'...'` when it holds control characters.
pub fn double_quote_value(text: &str) -> String {
    if needs_ansi_c_quoting(text) {
        return ansi_c_quote(text);
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if matches!(c, '"' | '$' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A value as `${v@Q}` prints it: `'...'` (with `'\''` for quotes), or
/// `$'...'` when it holds control characters.
pub fn single_quote_value(text: &str) -> String {
    if needs_ansi_c_quoting(text) {
        return ansi_c_quote(text);
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Bash's `sh_contains_shell_metas`, which decides whether an associative
/// key is quoted in `declare -p` output.
fn contains_shell_metas(text: &str) -> bool {
    text.chars().enumerate().any(|(i, c)| match c {
        ' ' | '\t' | '\n' | '\'' | '"' | '\\' | '|' | '&' | ';' | '(' | ')' | '<' | '>' | '!'
        | '{' | '}' | '*' | '[' | '?' | ']' | '^' | '$' | '`' => true,
        '~' | '#' => i == 0,
        _ => false,
    })
}

/// An associative key as `declare -p` prints it.
pub fn quote_assoc_key(key: &str) -> String {
    if needs_ansi_c_quoting(key) {
        ansi_c_quote(key)
    } else if contains_shell_metas(key) || key == "@" || key == "*" {
        double_quote_value(key)
    } else {
        key.to_string()
    }
}

/// The `(...)` part of `declare -p` for an array value: `([0]="a" [1]="b")`
/// for an indexed array, `([k]="v" )` for an associative one (bash leaves the
/// trailing space).
pub fn array_assignment_body(value: &VarValue) -> String {
    match value {
        VarValue::Indexed(a) => {
            let items: Vec<String> = a
                .iter()
                .map(|(i, v)| format!("[{}]={}", i, double_quote_value(v)))
                .collect();
            format!("({})", items.join(" "))
        }
        VarValue::Assoc(m) => {
            let mut out = String::from("(");
            for (k, v) in m.iter() {
                out.push_str(&format!(
                    "[{}]={} ",
                    quote_assoc_key(k),
                    double_quote_value(v)
                ));
            }
            out.push(')');
            out
        }
        _ => String::new(),
    }
}

/// One `declare -p` line for `name`: `declare -ix n="5"`, `declare -a a=(...)`,
/// `declare -- x="1"`, or just `declare -i n` for a declared but unset one.
pub fn declare_line(name: &str, var: &Variable) -> String {
    let letters = var.attr_letters();
    let flags = if letters.is_empty() {
        "--".to_string()
    } else {
        format!("-{letters}")
    };
    match &var.value {
        VarValue::Unset => format!("declare {flags} {name}"),
        VarValue::Scalar(s) => format!("declare {flags} {name}={}", double_quote_value(s)),
        array => format!("declare {flags} {name}={}", array_assignment_body(array)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_array_resolves_negative_subscripts_from_the_end() {
        let mut a = IndexedArray::new();
        a.set(2, "x".into());
        a.set(5, "y".into());
        assert_eq!(a.resolve(-1), Some(5));
        assert_eq!(a.resolve(-6), Some(0));
        assert_eq!(a.resolve(-7), None);
        assert_eq!(a.next_index(), Some(6));
        assert!(!a.is_dense());
    }

    #[test]
    fn assoc_listing_matches_bash_hash_order() {
        // Verified against bash 5.2: declare -A m=([b]=2 [a]=1 [c]=3 [zz]=1
        // [hello]=2); echo ${!m[@]}  →  hello c b a zz
        let mut m = AssocArray::new();
        for k in ["b", "a", "c", "zz", "hello"] {
            m.insert(k.into(), "v".into());
        }
        assert_eq!(m.keys(), vec!["hello", "c", "b", "a", "zz"]);
        // Replacing a value keeps the key's place.
        m.insert("b".into(), "new".into());
        assert_eq!(m.keys(), vec!["hello", "c", "b", "a", "zz"]);
    }

    #[test]
    fn random_sequence_matches_bash_for_a_seed() {
        // bash 5.2: RANDOM=42; echo $RANDOM $RANDOM $RANDOM  →  17772 26794 1435
        let vars = DynamicVars::new();
        vars.seed_random(42);
        let got: Vec<u32> = (0..3).map(|_| vars.random()).collect();
        assert_eq!(got, vec![17772, 26794, 1435]);
    }

    fn state() -> ShellState {
        ShellState::new(false)
    }

    #[test]
    fn assignment_in_a_function_creates_a_global() {
        let mut state = state();
        state.push_local_scope();
        state.set_var("jsh_scope_new", "1");
        state.pop_local_scope();
        assert_eq!(state.get_var("jsh_scope_new"), Some("1"));
    }

    #[test]
    fn assignment_updates_the_nearest_scope_holding_the_name() {
        let mut state = state();
        state.set_var("jsh_scope_x", "global");
        state.push_local_scope();
        state.declare_local("jsh_scope_x").unwrap();
        state.set_var("jsh_scope_x", "outer-local");
        // A callee without its own local assigns the caller's.
        state.push_local_scope();
        state.set_var("jsh_scope_x", "from-callee");
        state.pop_local_scope();
        assert_eq!(state.get_var("jsh_scope_x"), Some("from-callee"));
        state.pop_local_scope();
        assert_eq!(state.get_var("jsh_scope_x"), Some("global"));
    }

    #[test]
    fn unset_keeps_an_own_local_declared_but_removes_a_callers() {
        let mut state = state();
        state.set_var("jsh_scope_u", "global");
        state.push_local_scope();
        state.declare_local("jsh_scope_u").unwrap();
        state.set_var("jsh_scope_u", "local");
        state.unset_var("jsh_scope_u");
        // Still shadowing the global, as an unset local.
        assert_eq!(state.get_var("jsh_scope_u"), None);
        state.set_var("jsh_scope_u", "again");
        state.push_local_scope();
        state.unset_var("jsh_scope_u");
        // The caller's local is gone: the global shows through.
        assert_eq!(state.get_var("jsh_scope_u"), Some("global"));
        state.pop_local_scope();
        state.pop_local_scope();
        assert_eq!(state.get_var("jsh_scope_u"), Some("global"));
    }

    #[test]
    fn temporary_scope_holds_prefix_assignments_for_one_command() {
        let mut state = state();
        state.set_var("jsh_scope_t", "before");
        state.push_temp_scope();
        assert!(state.set_temp_var("jsh_scope_t", "during"));
        assert_eq!(state.exported_value("jsh_scope_t"), Some("during"));
        // `local` inside the command's function goes beneath it, so the
        // temporary scope alone does not count as a function.
        assert!(!state.in_function());
        state.pop_local_scope();
        assert_eq!(state.get_var("jsh_scope_t"), Some("before"));
        assert_eq!(state.exported_value("jsh_scope_t"), None);
    }

    #[test]
    fn readonly_refusal_is_recorded_for_the_executor() {
        let mut state = state();
        state.set_var("jsh_ro", "1");
        state.set_attrs("jsh_ro", VarAttrs::READONLY, VarAttrs::NONE);
        state.assignment_failed = false;
        state.set_var("jsh_ro", "2");
        assert!(state.take_assignment_failure());
        assert!(!state.take_assignment_failure(), "taking clears the flag");
        assert_eq!(state.get_var("jsh_ro"), Some("1"));
        assert!(state.unset_variable("jsh_ro").is_err());
    }

    #[test]
    fn namerefs_assign_through_and_detect_cycles() {
        let mut state = state();
        state.set_var("jsh_target", "old");
        state.set_var("jsh_ref", "jsh_target");
        state.set_attrs("jsh_ref", VarAttrs::NAMEREF, VarAttrs::NONE);
        state.set_var("jsh_ref", "new");
        assert_eq!(state.get_var("jsh_target"), Some("new"));
        state.set_var("jsh_loop_a", "jsh_loop_b");
        state.set_var("jsh_loop_b", "jsh_loop_a");
        state.set_attrs("jsh_loop_a", VarAttrs::NAMEREF, VarAttrs::NONE);
        state.set_attrs("jsh_loop_b", VarAttrs::NAMEREF, VarAttrs::NONE);
        assert!(state.resolve_nameref("jsh_loop_a").is_err());
        assert_eq!(state.get_var("jsh_loop_a"), None);
    }

    #[test]
    fn sparse_arrays_keep_holes_and_append_after_the_last_index() {
        let mut state = state();
        state.set_array("jsh_arr", vec!["a".into(), "b".into(), "c".into()]);
        state.unset_element("jsh_arr", "1").unwrap();
        state.append_array("jsh_arr", vec!["d".into()]);
        assert_eq!(state.array_keys("jsh_arr"), vec!["0", "2", "3"]);
        assert_eq!(
            state.get_array_element("jsh_arr", "-1").as_deref(),
            Some("d")
        );
        assert_eq!(state.array_length("jsh_arr"), 3);
    }

    #[test]
    fn assoc_listing_survives_table_growth_like_bash() {
        // bash 5.2: declare -A m; for i in {1..3000}; do m[k$i]=1; done;
        // echo "${!m[@]}" | tr ' ' '\n' | head -3  →  k1698 k1699 k1696
        let mut m = AssocArray::new();
        for i in 1..=3000 {
            m.insert(format!("k{i}"), "1".into());
        }
        assert_eq!(&m.keys()[..3], ["k1698", "k1699", "k1696"]);
    }

    #[test]
    fn quoting_follows_bash() {
        assert_eq!(double_quote_value("a\"b$c`d\\e"), "\"a\\\"b\\$c\\`d\\\\e\"");
        assert_eq!(double_quote_value("tab\there"), "$'tab\\there'");
        assert_eq!(single_quote_value("it's"), "'it'\\''s'");
        assert_eq!(quote_assoc_key("b c"), "\"b c\"");
        assert_eq!(quote_assoc_key("a"), "a");
        assert_eq!(VarAttrs::from_letters("xri").unwrap().letters(), "irx");
    }
}
