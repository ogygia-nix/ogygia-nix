//! List builtins.

use std::collections::HashMap;

use crate::builtins::Builtin;
use crate::eval::Eval;
use crate::symbol::Sym;
use crate::value::Entry;
use crate::value::R;
use crate::value::Value;
use crate::value::eval_err;
use crate::value::type_err;

pub fn head<'a>(ev: &Eval<'a>, list: Value<'a>) -> R<'a> {
    match ev.force_list(list)?.first() {
        Some(v) => ev.force(*v),
        None => eval_err("'builtins.head' called on an empty list"),
    }
}

pub fn tail<'a>(ev: &Eval<'a>, list: Value<'a>) -> R<'a> {
    match ev.force_list(list)? {
        [] => eval_err("'builtins.tail' called on an empty list"),
        [_, rest @ ..] => Ok(ev.list(rest)),
    }
}

pub fn length<'a>(ev: &Eval<'a>, list: Value<'a>) -> R<'a> {
    Ok(Value::Int(ev.force_list(list)?.len() as i64))
}

pub fn elem_at<'a>(ev: &Eval<'a>, list: Value<'a>, n: Value<'a>) -> R<'a> {
    let n = ev.force_int(n)?;
    let list = ev.force_list(list)?;
    match usize::try_from(n).ok().and_then(|i| list.get(i)) {
        Some(v) => ev.force(*v),
        None => eval_err(format!(
            "list index {n} is out of bounds for list of length {}",
            list.len()
        )),
    }
}

pub fn elem<'a>(ev: &Eval<'a>, x: Value<'a>, list: Value<'a>) -> R<'a> {
    for item in ev.force_list(list)? {
        if ev.eq_elem(x, *item)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub fn map<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let items = ev.force_list(list)?;
    if items.is_empty() {
        return ev.force(list);
    }
    let f = ev.force_function(f)?;
    let out: Vec<Value<'a>> = items.iter().map(|x| ev.lazy_app(f, *x)).collect();
    Ok(ev.list(&out))
}

pub fn filter<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let items = ev.force_list(list)?;
    if items.is_empty() {
        return ev.force(list);
    }
    let f = ev.force_function(f)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if ev.force_bool(ev.call(f, &[*item], None)?)? {
            out.push(*item);
        }
    }
    if out.len() == items.len() {
        return ev.force(list);
    }
    Ok(ev.list(&out))
}

pub fn all<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    for item in ev.force_list(list)? {
        if !ev.force_bool(ev.call(f, &[*item], None)?)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

pub fn any<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    for item in ev.force_list(list)? {
        if ev.force_bool(ev.call(f, &[*item], None)?)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub fn concat_lists<'a>(ev: &Eval<'a>, lists: Value<'a>) -> R<'a> {
    let mut out = Vec::new();
    for l in ev.force_list(lists)? {
        out.extend_from_slice(ev.force_list(*l)?);
    }
    Ok(ev.list(&out))
}

pub fn concat_map<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    let mut out = Vec::new();
    for item in ev.force_list(list)? {
        out.extend_from_slice(ev.force_list(ev.call(f, &[*item], None)?)?);
    }
    Ok(ev.list(&out))
}

pub fn foldl<'a>(ev: &Eval<'a>, f: Value<'a>, init: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    let mut acc = init;
    let list = ev.force_list(list)?;
    if list.is_empty() {
        return ev.force(acc);
    }
    for item in list {
        acc = ev.call(f, &[acc, *item], None)?;
    }
    Ok(acc)
}

pub fn gen_list<'a>(ev: &Eval<'a>, f: Value<'a>, n: Value<'a>) -> R<'a> {
    let n = ev.force_int(n)?;
    if n < 0 {
        return eval_err(format!("cannot create list of size {n}"));
    }
    let f = ev.force_function(f)?;
    let mut out: Vec<Value<'a>> = Vec::new();
    let reserved = usize::try_from(n)
        .ok()
        .is_some_and(|n| out.try_reserve_exact(n).is_ok());
    if !reserved {
        return eval_err("out of memory");
    }
    out.extend((0..n).map(|i| ev.lazy_app(f, Value::Int(i))));
    Ok(ev.list(&out))
}

pub fn partition<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    let mut right = Vec::new();
    let mut wrong = Vec::new();
    for item in ev.force_list(list)? {
        let item = ev.force(*item)?;
        if ev.force_bool(ev.call(f, &[item], None)?)? {
            right.push(item);
        } else {
            wrong.push(item);
        }
    }
    Ok(ev.attrs(vec![
        ev.entry("right", ev.list(&right)),
        ev.entry("wrong", ev.list(&wrong)),
    ]))
}

pub fn group_by<'a>(ev: &Eval<'a>, f: Value<'a>, list: Value<'a>) -> R<'a> {
    let f = ev.force_function(f)?;
    let mut groups: HashMap<Sym, Vec<Value<'a>>> = HashMap::new();
    for item in ev.force_list(list)? {
        let key = ev.force_str_no_ctx(ev.call(f, &[*item], None)?)?;
        let key = ev.ctx.interner.intern_bytes(key);
        groups.entry(key).or_default().push(*item);
    }
    let out = groups
        .into_iter()
        .map(|(name, items)| Entry {
            name,
            value: ev.list(&items),
            pos: None,
        })
        .collect();
    Ok(ev.attrs(out))
}

/// A stable merge sort whose comparator may fail.
pub fn sort<'a>(ev: &Eval<'a>, cmp: Value<'a>, list: Value<'a>) -> R<'a> {
    let mut items = ev.force_list(list)?.to_vec();
    if items.is_empty() {
        return Ok(ev.list(&items));
    }
    let cmp = ev.force_function(cmp)?;
    for item in &mut items {
        *item = ev.force(*item)?;
    }
    if items.len() == 1 {
        return Ok(ev.list(&items));
    }
    // Nix compares directly, without a call, when the comparator is `lessThan`.
    let direct = matches!(cmp, Value::PrimOp(Builtin::LessThan));
    let less = |a: Value<'a>, b: Value<'a>| -> R<'a, bool> {
        if direct {
            return ev.less_than(a, b);
        }
        ev.force_bool(ev.call(cmp, &[a, b], None)?)
    };
    let len = items.len();
    PeekSort {
        buf: items.clone(),
        less: &less,
    }
    .sort_range(&mut items, 0, len, 0, len)?;
    Ok(ev.list(&items))
}

/// The stable natural merge sort (PeekSort) Nix's `builtins.sort` uses. A
/// comparator that is not a strict weak ordering gives an order that depends
/// on exactly which comparisons are made, so this makes Nix's comparisons.
struct PeekSort<'s, 'a> {
    buf: Vec<Value<'a>>,
    less: &'s dyn Fn(Value<'a>, Value<'a>) -> R<'a, bool>,
}

impl<'a> PeekSort<'_, 'a> {
    /// Whether `next` continues a run after `prev`: a weakly increasing run
    /// needs `!(next < prev)`, a strictly decreasing one `next < prev`.
    fn in_run(&self, decreasing: bool, prev: Value<'a>, next: Value<'a>) -> R<'a, bool> {
        Ok((self.less)(next, prev)? == decreasing)
    }

    /// The end of the run starting at `begin`, scanning forwards to `end`.
    fn run_end(
        &self,
        v: &[Value<'a>],
        decreasing: bool,
        mut begin: usize,
        end: usize,
    ) -> R<'a, usize> {
        if begin == end {
            return Ok(begin);
        }
        while begin + 1 != end && self.in_run(decreasing, v[begin], v[begin + 1])? {
            begin += 1;
        }
        Ok(begin + 1)
    }

    /// The start of the run ending at `end`, scanning backwards to `begin`.
    fn run_start(
        &self,
        v: &[Value<'a>],
        decreasing: bool,
        begin: usize,
        mut end: usize,
    ) -> R<'a, usize> {
        if begin == end {
            return Ok(end);
        }
        while end - 1 > begin && self.in_run(decreasing, v[end - 2], v[end - 1])? {
            end -= 1;
        }
        Ok(end - 1)
    }

    fn insertion_sort(&self, v: &mut [Value<'a>]) -> R<'a, ()> {
        for i in 1..v.len() {
            let mut j = i;
            while j > 0 && (self.less)(v[j], v[j - 1])? {
                v.swap(j, j - 1);
                j -= 1;
            }
        }
        Ok(())
    }

    /// Merges the sorted `v[begin..mid]` and `v[mid..end]`.
    fn merge(&mut self, v: &mut [Value<'a>], begin: usize, mid: usize, end: usize) -> R<'a, ()> {
        let (left, n) = (mid - begin, end - begin);
        self.buf[..n].copy_from_slice(&v[begin..end]);
        let (mut l, mut r, mut out) = (0, left, begin);
        while l < left && r < n {
            if (self.less)(self.buf[r], self.buf[l])? {
                v[out] = self.buf[r];
                r += 1;
            } else {
                v[out] = self.buf[l];
                l += 1;
            }
            out += 1;
        }
        let rest = if l < left { l..left } else { r..n };
        v[out..end].copy_from_slice(&self.buf[rest]);
        Ok(())
    }

    /// Sorts `v[begin..end]`, given that `v[begin..left_end]` and
    /// `v[right_begin..end]` are already sorted.
    fn sort_range(
        &mut self,
        v: &mut [Value<'a>],
        begin: usize,
        end: usize,
        left_end: usize,
        right_begin: usize,
    ) -> R<'a, ()> {
        if left_end == end || right_begin == begin {
            return Ok(());
        }
        let n = end - begin;
        if n <= 16 {
            return self.insertion_sort(&mut v[begin..end]);
        }
        let mid = begin + n / 2;
        if mid <= left_end {
            self.sort_range(v, left_end, end, left_end + 1, right_begin)?;
            return self.merge(v, begin, left_end, end);
        }
        if mid >= right_begin {
            self.sort_range(v, begin, right_begin, left_end, right_begin - 1)?;
            return self.merge(v, begin, right_begin, end);
        }
        // The run containing `v[mid - 1]`, reversed if decreasing.
        let decreasing = (self.less)(v[mid], v[mid - 1])?;
        let i = self.run_start(v, decreasing, left_end, mid)?;
        let j = self.run_end(v, decreasing, mid - 1, right_begin)?;
        if decreasing {
            v[i..j].reverse();
        }
        if i == begin && j == end {
            return Ok(());
        }
        if mid - i < j - mid {
            // When `i == begin` the left part is empty and returns at once.
            self.sort_range(v, begin, i, left_end, i.wrapping_sub(1))?;
            self.sort_range(v, i, end, j, right_begin)?;
            self.merge(v, begin, i, end)
        } else {
            self.sort_range(v, begin, j, left_end, i)?;
            self.sort_range(v, j, end, j + 1, right_begin)?;
            self.merge(v, begin, j, end)
        }
    }
}

pub fn generic_closure<'a>(ev: &Eval<'a>, set: Value<'a>) -> R<'a> {
    let s = &ev.ctx.syms;
    let set = ev.force_attrs(set)?;
    let Some(start) = set.get(s.start_set) else {
        return type_err("attribute 'startSet' required");
    };
    let start = ev.force_list(start)?;
    if start.is_empty() {
        return Ok(ev.list(&[]));
    }
    let Some(op) = set.get(s.operator) else {
        return type_err("attribute 'operator' required");
    };
    let op = ev.force_function(op)?;
    let mut queue: std::collections::VecDeque<Value<'a>> = start.iter().copied().collect();
    let mut seen = KeySet::default();
    let mut out = Vec::new();
    while let Some(item) = queue.pop_front() {
        let attrs = ev.force_attrs(item)?;
        let Some(key) = attrs.get(s.key) else {
            return type_err("attribute 'key' required");
        };
        let key = ev.force(key)?;
        if !seen.insert(ev, key)? {
            continue;
        }
        out.push(item);
        let next = ev.call(op, &[item], None)?;
        for &elem in ev.force_list(next)? {
            ev.force(elem)?;
            queue.push_back(elem);
        }
    }
    Ok(ev.list(&out))
}

/// The keys `genericClosure` has seen, kept as Nix keeps them: a libstdc++
/// `std::set` ordered by `<`. Mirroring its red-black tree node for node makes
/// the same pairs get compared, which decides both which keys fail to compare
/// and which count as equivalent (NaN is equivalent to every number).
#[derive(Default)]
struct KeySet<'a> {
    nodes: Vec<KeyNode<'a>>,
    root: Option<usize>,
    leftmost: usize,
}

struct KeyNode<'a> {
    key: Value<'a>,
    parent: Option<usize>,
    left: Option<usize>,
    right: Option<usize>,
    red: bool,
}

impl<'a> KeySet<'a> {
    /// Inserts `key` unless an equivalent key is present; returns whether it
    /// was inserted.
    fn insert(&mut self, ev: &Eval<'a>, key: Value<'a>) -> R<'a, bool> {
        // `_M_get_insert_unique_pos`.
        let mut x = self.root;
        let mut y = None;
        let mut comp = true;
        while let Some(n) = x {
            y = Some(n);
            comp = ev.less_than(key, self.nodes[n].key)?;
            x = if comp {
                self.nodes[n].left
            } else {
                self.nodes[n].right
            };
        }
        if let Some(p) = y {
            let j = if comp {
                (p != self.leftmost).then(|| self.predecessor(p))
            } else {
                Some(p)
            };
            if let Some(j) = j
                && !ev.less_than(self.nodes[j].key, key)?
            {
                return Ok(false);
            }
        }
        // `_M_insert_` compares `key` with `y` again to choose the side; the
        // comparison is pure, so `comp` already holds its result.
        let z = self.nodes.len();
        self.nodes.push(KeyNode {
            key,
            parent: y,
            left: None,
            right: None,
            red: true,
        });
        match y {
            None => {
                self.root = Some(z);
                self.leftmost = z;
            }
            Some(p) if comp => {
                self.nodes[p].left = Some(z);
                if p == self.leftmost {
                    self.leftmost = z;
                }
            }
            Some(p) => self.nodes[p].right = Some(z),
        }
        self.rebalance(z);
        Ok(true)
    }

    fn predecessor(&self, mut n: usize) -> usize {
        if let Some(mut l) = self.nodes[n].left {
            while let Some(r) = self.nodes[l].right {
                l = r;
            }
            return l;
        }
        while let Some(p) = self.nodes[n].parent {
            if self.nodes[p].left != Some(n) {
                return p;
            }
            n = p;
        }
        unreachable!("the leftmost node has no predecessor")
    }

    fn is_red(&self, n: Option<usize>) -> bool {
        n.is_some_and(|n| self.nodes[n].red)
    }

    /// `_Rb_tree_insert_and_rebalance` after linking `x`.
    fn rebalance(&mut self, mut x: usize) {
        while let Some(xp) = self.nodes[x].parent
            && self.nodes[xp].red
        {
            let xpp = self.nodes[xp].parent.expect("a red node is not the root");
            let parent_is_left = self.nodes[xpp].left == Some(xp);
            let uncle = if parent_is_left {
                self.nodes[xpp].right
            } else {
                self.nodes[xpp].left
            };
            if let Some(u) = uncle
                && self.is_red(uncle)
            {
                self.nodes[xp].red = false;
                self.nodes[u].red = false;
                self.nodes[xpp].red = true;
                x = xpp;
                continue;
            }
            if parent_is_left {
                if self.nodes[xp].right == Some(x) {
                    x = xp;
                    self.rotate_left(x);
                }
            } else if self.nodes[xp].left == Some(x) {
                x = xp;
                self.rotate_right(x);
            }
            let xp = self.nodes[x].parent.expect("x has a parent");
            self.nodes[xp].red = false;
            self.nodes[xpp].red = true;
            if parent_is_left {
                self.rotate_right(xpp);
            } else {
                self.rotate_left(xpp);
            }
        }
        if let Some(root) = self.root {
            self.nodes[root].red = false;
        }
    }

    fn replace_child(&mut self, parent: Option<usize>, old: usize, new: usize) {
        match parent {
            None => self.root = Some(new),
            Some(p) if self.nodes[p].left == Some(old) => self.nodes[p].left = Some(new),
            Some(p) => self.nodes[p].right = Some(new),
        }
    }

    fn rotate_left(&mut self, x: usize) {
        let y = self.nodes[x]
            .right
            .expect("rotating left needs a right child");
        self.nodes[x].right = self.nodes[y].left;
        if let Some(c) = self.nodes[y].left {
            self.nodes[c].parent = Some(x);
        }
        let xp = self.nodes[x].parent;
        self.nodes[y].parent = xp;
        self.replace_child(xp, x, y);
        self.nodes[y].left = Some(x);
        self.nodes[x].parent = Some(y);
    }

    fn rotate_right(&mut self, x: usize) {
        let y = self.nodes[x]
            .left
            .expect("rotating right needs a left child");
        self.nodes[x].left = self.nodes[y].right;
        if let Some(c) = self.nodes[y].right {
            self.nodes[c].parent = Some(x);
        }
        let xp = self.nodes[x].parent;
        self.nodes[y].parent = xp;
        self.replace_child(xp, x, y);
        self.nodes[y].right = Some(x);
        self.nodes[x].parent = Some(y);
    }
}
