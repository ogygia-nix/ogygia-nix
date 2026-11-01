//! The `builtins` set and the global scope.

mod attrs;
mod context;
mod derivation;
pub(crate) mod flake;
mod fs;
mod grisu2;
pub(crate) mod json;
mod lists;
mod misc;
pub(crate) mod regex;
mod strings;
pub(crate) mod url;
mod xml;

use std::cell::Cell;

use enum_map::Enum;
use enum_map::EnumMap;
use strum::EnumString;
use strum::IntoStaticStr;

use crate::eval::Eval;
use crate::store::STORE_DIR;
use crate::value::PrimOpFn;
use crate::value::PrimOpFn::F1;
use crate::value::PrimOpFn::F2;
use crate::value::PrimOpFn::F3;
use crate::value::Thunk;
use crate::value::ThunkState;
use crate::value::Value;

/// The Nix version this evaluator reports, matching the Nix its equivalence
/// tests run against when built by the flake.
pub const NIX_VERSION: &str = match option_env!("OGYGIA_NIX_EVAL_NIX_VERSION") {
    Some(v) => v,
    None => "2.36.0pre",
};

/// A builtin, named as in the `builtins` set.
#[derive(Clone, Copy, PartialEq, Eq, Enum, EnumString, IntoStaticStr)]
#[strum(serialize_all = "camelCase")]
pub enum Builtin {
    Abort,
    Add,
    AddDrvOutputDependencies,
    AddErrorContext,
    All,
    Any,
    AppendContext,
    AttrNames,
    AttrValues,
    BaseNameOf,
    BitAnd,
    BitOr,
    BitXor,
    Break,
    Builtins,
    CatAttrs,
    Ceil,
    CompareVersions,
    ConcatLists,
    ConcatMap,
    ConcatStringsSep,
    ConvertHash,
    CurrentSystem,
    CurrentTime,
    DeepSeq,
    Derivation,
    DerivationStrict,
    DirOf,
    Div,
    Elem,
    ElemAt,
    False,
    FetchGit,
    FetchMercurial,
    FetchTarball,
    FetchTree,
    Fetchurl,
    Filter,
    FilterSource,
    FindFile,
    FlakeRefToString,
    Floor,
    #[strum(serialize = "foldl'")]
    FoldlStrict,
    #[strum(serialize = "fromJSON")]
    FromJson,
    #[strum(serialize = "fromTOML")]
    FromToml,
    FunctionArgs,
    GenList,
    GenericClosure,
    GetAttr,
    GetContext,
    GetEnv,
    GetFlake,
    GroupBy,
    HasAttr,
    HasContext,
    HashFile,
    HashString,
    Head,
    Import,
    IntersectAttrs,
    IsAttrs,
    IsBool,
    IsFloat,
    IsFunction,
    IsInt,
    IsList,
    IsNull,
    IsPath,
    IsString,
    LangVersion,
    Length,
    LessThan,
    ListToAttrs,
    Map,
    MapAttrs,
    Match,
    Mul,
    NixPath,
    NixVersion,
    Null,
    ParseDrvName,
    ParseFlakeRef,
    Partition,
    Path,
    PathExists,
    Placeholder,
    ReadDir,
    ReadFile,
    ReadFileType,
    RemoveAttrs,
    ReplaceStrings,
    ScopedImport,
    Seq,
    Sort,
    Split,
    SplitVersion,
    StoreDir,
    StorePath,
    StringLength,
    Sub,
    Substring,
    Tail,
    Throw,
    ToFile,
    #[strum(serialize = "toJSON")]
    ToJson,
    ToPath,
    ToString,
    #[strum(serialize = "toXML")]
    ToXml,
    Trace,
    TraceVerbose,
    True,
    TryEval,
    TypeOf,
    UnsafeDiscardOutputDependency,
    UnsafeDiscardStringContext,
    UnsafeGetAttrPos,
    Warn,
    ZipAttrsWith,
}

/// What a builtin is.
pub(crate) enum Kind {
    PrimOp(PrimOpFn),
    /// A value built per session.
    Const(for<'a> fn(&Eval<'a>) -> Value<'a>),
    /// The `builtins` set itself.
    Builtins,
}

impl Builtin {
    pub fn name(self) -> &'static str {
        self.into()
    }

    /// The builtin that `name` refers to in the global scope, if any.
    pub fn in_scope(name: &str, pure: bool) -> Option<Builtin> {
        let bare = name.strip_prefix("__");
        let b: Builtin = bare.unwrap_or(name).parse().ok()?;
        (b.global() == bare.is_none() && !(pure && b.impure()) && !b.flakes()).then_some(b)
    }

    /// Whether the `builtins` set has this builtin only with the `flakes`
    /// experimental feature enabled, and the global scope never does.
    fn flakes(self) -> bool {
        matches!(self, Builtin::FetchTree)
    }

    /// Whether the global scope has this builtin without the `__` prefix.
    fn global(self) -> bool {
        use Builtin::*;
        matches!(
            self,
            Abort
                | BaseNameOf
                | Break
                | Builtins
                | Derivation
                | DerivationStrict
                | DirOf
                | False
                | FetchGit
                | FetchMercurial
                | FetchTarball
                | FromToml
                | Import
                | IsNull
                | Map
                | Null
                | Placeholder
                | RemoveAttrs
                | ScopedImport
                | Throw
                | ToString
                | True
        )
    }

    /// Whether pure evaluation leaves this builtin out.
    fn impure(self) -> bool {
        matches!(self, Builtin::CurrentSystem | Builtin::CurrentTime)
    }

    pub(crate) fn kind(self) -> Kind {
        use Builtin::*;
        match self {
            Abort => Kind::PrimOp(F1(misc::abort)),
            Add => Kind::PrimOp(F2(misc::add)),
            AddDrvOutputDependencies => Kind::PrimOp(F1(context::add_drv_output_dependencies)),
            AddErrorContext => Kind::PrimOp(F2(misc::add_error_context)),
            All => Kind::PrimOp(F2(lists::all)),
            Any => Kind::PrimOp(F2(lists::any)),
            AppendContext => Kind::PrimOp(F2(context::append_context)),
            AttrNames => Kind::PrimOp(F1(attrs::attr_names)),
            AttrValues => Kind::PrimOp(F1(attrs::attr_values)),
            BaseNameOf => Kind::PrimOp(F1(strings::base_name_of)),
            BitAnd => Kind::PrimOp(F2(misc::bit_and)),
            BitOr => Kind::PrimOp(F2(misc::bit_or)),
            BitXor => Kind::PrimOp(F2(misc::bit_xor)),
            Break => Kind::PrimOp(F1(misc::break_)),
            Builtins => Kind::Builtins,
            CatAttrs => Kind::PrimOp(F2(attrs::cat_attrs)),
            Ceil => Kind::PrimOp(F1(misc::ceil)),
            CompareVersions => Kind::PrimOp(F2(strings::compare_versions)),
            ConcatLists => Kind::PrimOp(F1(lists::concat_lists)),
            ConcatMap => Kind::PrimOp(F2(lists::concat_map)),
            ConcatStringsSep => Kind::PrimOp(F2(strings::concat_strings_sep)),
            ConvertHash => Kind::PrimOp(F1(strings::convert_hash)),
            CurrentSystem => Kind::Const(|ev| ev.string(&ev.settings.current_system)),
            CurrentTime => Kind::Const(|_| Value::Int(current_time())),
            DeepSeq => Kind::PrimOp(F2(misc::deep_seq)),
            Derivation => Kind::Const(derivation::derivation),
            DerivationStrict => Kind::PrimOp(F1(derivation::derivation_strict)),
            DirOf => Kind::PrimOp(F1(strings::dir_of)),
            Div => Kind::PrimOp(F2(misc::div)),
            Elem => Kind::PrimOp(F2(lists::elem)),
            ElemAt => Kind::PrimOp(F2(lists::elem_at)),
            False => Kind::Const(|_| Value::Bool(false)),
            FetchGit => Kind::PrimOp(F1(fs::fetch_git)),
            FetchMercurial => Kind::PrimOp(F1(fs::fetch_mercurial)),
            FetchTarball => Kind::PrimOp(F1(fs::fetch_tarball)),
            FetchTree => Kind::PrimOp(F1(fs::fetch_tree)),
            Fetchurl => Kind::PrimOp(F1(fs::fetchurl)),
            Filter => Kind::PrimOp(F2(lists::filter)),
            FilterSource => Kind::PrimOp(F2(fs::filter_source)),
            FindFile => Kind::PrimOp(F2(fs::find_file)),
            FlakeRefToString => Kind::PrimOp(F1(flake::flake_ref_to_string)),
            Floor => Kind::PrimOp(F1(misc::floor)),
            FoldlStrict => Kind::PrimOp(F3(lists::foldl)),
            FromJson => Kind::PrimOp(F1(json::from_json)),
            FromToml => Kind::PrimOp(F1(json::from_toml)),
            FunctionArgs => Kind::PrimOp(F1(misc::function_args)),
            GenList => Kind::PrimOp(F2(lists::gen_list)),
            GenericClosure => Kind::PrimOp(F1(lists::generic_closure)),
            GetAttr => Kind::PrimOp(F2(attrs::get_attr)),
            GetContext => Kind::PrimOp(F1(context::get_context)),
            GetEnv => Kind::PrimOp(F1(misc::get_env)),
            GetFlake => Kind::PrimOp(F1(flake::get_flake)),
            GroupBy => Kind::PrimOp(F2(lists::group_by)),
            HasAttr => Kind::PrimOp(F2(attrs::has_attr)),
            HasContext => Kind::PrimOp(F1(context::has_context)),
            HashFile => Kind::PrimOp(F2(fs::hash_file)),
            HashString => Kind::PrimOp(F2(strings::hash_string)),
            Head => Kind::PrimOp(F1(lists::head)),
            Import => Kind::PrimOp(F1(fs::import)),
            IntersectAttrs => Kind::PrimOp(F2(attrs::intersect_attrs)),
            IsAttrs => Kind::PrimOp(F1(misc::is_attrs)),
            IsBool => Kind::PrimOp(F1(misc::is_bool)),
            IsFloat => Kind::PrimOp(F1(misc::is_float)),
            IsFunction => Kind::PrimOp(F1(misc::is_function)),
            IsInt => Kind::PrimOp(F1(misc::is_int)),
            IsList => Kind::PrimOp(F1(misc::is_list)),
            IsNull => Kind::PrimOp(F1(misc::is_null)),
            IsPath => Kind::PrimOp(F1(misc::is_path)),
            IsString => Kind::PrimOp(F1(misc::is_string)),
            LangVersion => Kind::Const(|_| Value::Int(6)),
            Length => Kind::PrimOp(F1(lists::length)),
            LessThan => Kind::PrimOp(F2(misc::less_than)),
            ListToAttrs => Kind::PrimOp(F1(attrs::list_to_attrs)),
            Map => Kind::PrimOp(F2(lists::map)),
            MapAttrs => Kind::PrimOp(F2(attrs::map_attrs)),
            Match => Kind::PrimOp(F2(regex::match_)),
            Mul => Kind::PrimOp(F2(misc::mul)),
            NixPath => Kind::Const(nix_path),
            NixVersion => Kind::Const(|ev| ev.string(NIX_VERSION)),
            Null => Kind::Const(|_| Value::Null),
            ParseDrvName => Kind::PrimOp(F1(strings::parse_drv_name)),
            ParseFlakeRef => Kind::PrimOp(F1(flake::parse_flake_ref)),
            Partition => Kind::PrimOp(F2(lists::partition)),
            Path => Kind::PrimOp(F1(fs::path)),
            PathExists => Kind::PrimOp(F1(fs::path_exists)),
            Placeholder => Kind::PrimOp(F1(derivation::placeholder)),
            ReadDir => Kind::PrimOp(F1(fs::read_dir)),
            ReadFile => Kind::PrimOp(F1(fs::read_file)),
            ReadFileType => Kind::PrimOp(F1(fs::read_file_type)),
            RemoveAttrs => Kind::PrimOp(F2(attrs::remove_attrs)),
            ReplaceStrings => Kind::PrimOp(F3(strings::replace_strings)),
            ScopedImport => Kind::PrimOp(F2(fs::scoped_import)),
            Seq => Kind::PrimOp(F2(misc::seq)),
            Sort => Kind::PrimOp(F2(lists::sort)),
            Split => Kind::PrimOp(F2(regex::split)),
            SplitVersion => Kind::PrimOp(F1(strings::split_version)),
            StoreDir => Kind::Const(|ev| ev.string(STORE_DIR)),
            StorePath => Kind::PrimOp(F1(fs::store_path)),
            StringLength => Kind::PrimOp(F1(strings::string_length)),
            Sub => Kind::PrimOp(F2(misc::sub)),
            Substring => Kind::PrimOp(F3(strings::substring)),
            Tail => Kind::PrimOp(F1(lists::tail)),
            Throw => Kind::PrimOp(F1(misc::throw)),
            ToFile => Kind::PrimOp(F2(fs::to_file)),
            ToJson => Kind::PrimOp(F1(json::to_json)),
            ToPath => Kind::PrimOp(F1(fs::to_path)),
            ToString => Kind::PrimOp(F1(strings::to_string)),
            ToXml => Kind::PrimOp(F1(xml::to_xml)),
            Trace => Kind::PrimOp(F2(misc::trace)),
            TraceVerbose => Kind::PrimOp(F2(misc::trace_verbose)),
            True => Kind::Const(|_| Value::Bool(true)),
            TryEval => Kind::PrimOp(F1(misc::try_eval)),
            TypeOf => Kind::PrimOp(F1(misc::type_of)),
            UnsafeDiscardOutputDependency => {
                Kind::PrimOp(F1(context::unsafe_discard_output_dependency))
            }
            UnsafeDiscardStringContext => Kind::PrimOp(F1(context::unsafe_discard_string_context)),
            UnsafeGetAttrPos => Kind::PrimOp(F2(attrs::unsafe_get_attr_pos)),
            Warn => Kind::PrimOp(F2(misc::warn)),
            ZipAttrsWith => Kind::PrimOp(F2(attrs::zip_attrs_with)),
        }
    }
}

fn current_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn nix_path<'a>(ev: &Eval<'a>) -> Value<'a> {
    let entries: Vec<Value<'a>> = ev
        .settings
        .nix_path
        .iter()
        .map(|(prefix, path)| {
            ev.attrs(vec![
                ev.entry("prefix", ev.string(prefix)),
                ev.entry("path", ev.string(path)),
            ])
        })
        .collect();
    ev.list(&entries)
}

/// Build the value of every builtin for a session.
pub fn make_globals<'a>(ev: &Eval<'a>) -> EnumMap<Builtin, Value<'a>> {
    // `builtins.builtins` is the set itself.
    let self_ref: &'a Thunk<'a> = ev.bump.alloc(Thunk(Cell::new(ThunkState::Blackhole)));
    let globals = EnumMap::from_fn(|b: Builtin| match b.kind() {
        Kind::PrimOp(_) => Value::PrimOp(b),
        Kind::Const(f) => f(ev),
        Kind::Builtins => Value::Thunk(self_ref),
    });
    let entries = globals
        .iter()
        .filter(|(b, _)| !(ev.ctx.io.pure && b.impure()) && (ev.ctx.flakes || !b.flakes()))
        .map(|(b, v)| ev.entry(b.name(), *v))
        .collect();
    self_ref.0.set(ThunkState::Done(ev.attrs(entries)));
    globals
}
