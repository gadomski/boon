use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt::Display;

use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::content::{DECODERS, MEDIA_TYPES};
use crate::draft::{DRAFT2019, DRAFT2020, DRAFT4, DRAFT6, DRAFT7};
use crate::formats::FORMATS;
use crate::root::Root;
use crate::roots::Roots;
use crate::util::*;
use crate::*;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Draft {
    V4,
    V6,
    V7,
    V2019_09,
    V2020_12,
}

impl Draft {
    pub(crate) fn internal(&self) -> &'static crate::draft::Draft {
        match self {
            Draft::V4 => &DRAFT4,
            Draft::V6 => &DRAFT6,
            Draft::V7 => &DRAFT7,
            Draft::V2019_09 => &DRAFT2019,
            Draft::V2020_12 => &DRAFT2020,
        }
    }
    fn from_version(version: usize) -> Draft {
        match version {
            4 => Self::V4,
            6 => Self::V6,
            7 => Self::V7,
            2019 => Self::V2019_09,
            _ => Self::V2020_12,
        }
    }
}

// returns latest draft supported
impl Default for Draft {
    fn default() -> Self {
        Draft::V2020_12
    }
}

#[derive(Default)]
pub struct Compiler {
    roots: Roots,
    assert_format: bool,
    assert_content: bool,
    formats: HashMap<&'static str, Format>,
    decoders: HashMap<&'static str, Decoder>,
    media_types: HashMap<&'static str, MediaType>,
}

impl Compiler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_default_draft(&mut self, d: Draft) {
        self.roots.default_draft = d.internal()
    }

    pub fn enable_format_assertions(&mut self) {
        self.assert_format = true;
    }

    pub fn enable_content_assertions(&mut self) {
        self.assert_content = true;
    }

    pub fn register_url_loader(&mut self, scheme: &'static str, url_loader: Box<dyn UrlLoader>) {
        self.roots.loader.register(scheme, url_loader);
    }

    pub fn register_format(&mut self, name: &'static str, format: Format) {
        self.formats.insert(name, format);
    }

    pub fn register_decoder(&mut self, content_encoding: &'static str, decoder: Decoder) {
        self.decoders.insert(content_encoding, decoder);
    }

    pub fn register_media_type(&mut self, media_type: &'static str, validator: MediaType) {
        self.media_types.insert(media_type, validator);
    }

    pub fn add_resource(&mut self, url: &str, json: Value) -> Result<bool, CompileError> {
        let url = Url::parse(url).map_err(|e| CompileError::LoadUrlError {
            url: url.to_owned(),
            src: e.into(),
        })?;
        self.roots.or_insert(url, json)
    }

    pub fn compile(
        &mut self,
        target: &mut Schemas,
        mut loc: String,
    ) -> Result<SchemaIndex, CompileError> {
        if loc.rfind('#').is_none() {
            loc.push('#');
        }

        let mut queue = VecDeque::new();
        let index = target.enqueue(&mut queue, loc);
        if queue.is_empty() {
            // already got compiled
            return Ok(SchemaIndex(index));
        }

        let mut sch_index = None;
        while let Some(loc) = queue.front() {
            let (url, ptr) = split(loc);
            let url = Url::parse(url).map_err(|e| CompileError::LoadUrlError {
                url: url.to_owned(),
                src: e.into(),
            })?;
            self.roots.or_load(url.clone())?;
            let root = self.roots.get(&url).unwrap();
            let v = root
                .lookup_ptr(ptr)
                .map_err(|_| CompileError::InvalidJsonPointer(loc.clone()))?;
            let Some(v) = v else {
                return Err(CompileError::JsonPointerNotFound(loc.to_owned()));
            };

            let sch = self.compile_one(target, v, loc.to_owned(), root, &mut queue)?;
            let loc = queue
                .pop_front()
                .ok_or(CompileError::Bug("queue must be non-empty".into()))?;
            let index = target.insert(loc, sch);
            sch_index = sch_index.or(Some(index));
        }
        sch_index.ok_or(CompileError::Bug("schema_index must exist".into()))
    }

    fn compile_one(
        &self,
        schemas: &Schemas,
        v: &Value,
        loc: String,
        root: &Root,
        queue: &mut VecDeque<String>,
    ) -> Result<Schema, CompileError> {
        let mut s = Schema::new(loc.clone());
        s.draft_version = root.draft.version;

        // we know it is already in queue, we just want to get its index
        s.index = schemas.enqueue(queue, loc.to_owned());
        s.resource = {
            let (_, ptr) = split(&loc);
            let base = root.base_url(ptr);
            let base_loc = root.resolve(base.as_str())?;
            schemas.enqueue(queue, base_loc)
        };

        // enqueue dynamicAnchors for compilation
        if s.index == s.resource && root.draft.version >= 2020 {
            let (url, ptr) = split(&loc);
            if let Some(res) = root.resource(ptr) {
                for danchor in &res.dynamic_anchors {
                    let danchor_ptr = res.anchors.get(danchor).unwrap();
                    let danchor_sch = schemas.enqueue(queue, format!("{url}#{danchor_ptr}"));
                    s.dynamic_anchors.insert(danchor.to_owned(), danchor_sch);
                }
            }
        }

        let obj = match v {
            Value::Object(obj) => obj,
            Value::Bool(b) => {
                // boolean schema
                s.boolean = Some(*b);
                return Ok(s);
            }
            _ => return Ok(s),
        };

        // helpers --
        let load_usize = |pname| {
            if let Some(Value::Number(n)) = obj.get(pname) {
                if n.is_u64() {
                    n.as_u64().map(|n| n as usize)
                } else {
                    n.as_f64()
                        .filter(|n| n.is_sign_positive() && n.fract() == 0.0)
                        .map(|n| n as usize)
                }
            } else {
                None
            }
        };
        let load_num = |pname| {
            if let Some(Value::Number(n)) = obj.get(pname) {
                Some(n.clone())
            } else {
                None
            }
        };
        let to_strings = |v: &Value| {
            if let Value::Array(a) = v {
                a.iter()
                    .filter_map(|t| {
                        if let Value::String(t) = t {
                            Some(t.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                vec![]
            }
        };
        let enqueue =
            |path, queue: &mut VecDeque<String>| schemas.enqueue(queue, format!("{loc}/{path}"));
        let enqueue_prop = |pname, queue: &mut VecDeque<String>| {
            if obj.contains_key(pname) {
                Some(schemas.enqueue(queue, format!("{loc}/{}", escape(pname))))
            } else {
                None
            }
        };
        let enquue_arr = |pname, queue: &mut VecDeque<String>| {
            if let Some(Value::Array(arr)) = obj.get(pname) {
                (0..arr.len())
                    .map(|i| schemas.enqueue(queue, format!("{loc}/{pname}/{i}")))
                    .collect()
            } else {
                Vec::new()
            }
        };
        let enqueue_map = |pname, queue: &mut VecDeque<String>| {
            if let Some(Value::Object(obj)) = obj.get(pname) {
                obj.keys()
                    .map(|k| {
                        (
                            k.clone(),
                            schemas.enqueue(queue, format!("{loc}/{pname}/{}", escape(k))),
                        )
                    })
                    .collect()
            } else {
                HashMap::new()
            }
        };
        let enqueue_ref =
            |pname, queue: &mut VecDeque<String>| -> Result<Option<usize>, CompileError> {
                if let Some(Value::String(ref_)) = obj.get(pname) {
                    let (_, ptr) = split(&loc);
                    let abs_ref =
                        root.base_url(ptr)
                            .join(ref_)
                            .map_err(|e| CompileError::ParseUrlError {
                                url: ref_.clone(),
                                src: e.into(),
                            })?;
                    let resolved_ref = root.resolve(abs_ref.as_str())?;
                    Ok(Some(schemas.enqueue(queue, resolved_ref)))
                } else {
                    Ok(None)
                }
            };

        // draft4 --
        if root.has_vocab("core") {
            s.ref_ = enqueue_ref("$ref", queue)?;
            if s.ref_.is_some() && root.draft.version < 2019 {
                // All other properties in a "$ref" object MUST be ignored
                return Ok(s);
            }
        }

        if root.has_vocab("applicator") {
            s.all_of = enquue_arr("allOf", queue);
            s.any_of = enquue_arr("anyOf", queue);
            s.one_of = enquue_arr("oneOf", queue);
            s.not = enqueue_prop("not", queue);

            if root.draft.version < 2020 {
                match obj.get("items") {
                    Some(Value::Array(_)) => {
                        s.items = Some(Items::SchemaRefs(enquue_arr("items", queue)));
                        s.additional_items = {
                            if let Some(Value::Bool(b)) = obj.get("additionalItems") {
                                Some(Additional::Bool(*b))
                            } else {
                                enqueue_prop("additionalItems", queue).map(Additional::SchemaRef)
                            }
                        };
                    }
                    _ => s.items = enqueue_prop("items", queue).map(Items::SchemaRef),
                }
            }

            s.properties = enqueue_map("properties", queue);
            s.pattern_properties = {
                let mut v = vec![];
                if let Some(Value::Object(obj)) = obj.get("patternProperties") {
                    for pname in obj.keys() {
                        let regex = Regex::new(pname).map_err(|_| CompileError::InvalidRegex {
                            url: format!("{loc}/patternProperties"),
                            regex: pname.clone(),
                        })?;
                        let sch = enqueue(format!("patternProperties/{}", escape(pname)), queue);
                        v.push((regex, sch));
                    }
                }
                v
            };

            s.additional_properties = {
                if let Some(Value::Bool(b)) = obj.get("additionalProperties") {
                    Some(Additional::Bool(*b))
                } else {
                    enqueue_prop("additionalProperties", queue).map(Additional::SchemaRef)
                }
            };

            if let Some(Value::Object(deps)) = obj.get("dependencies") {
                s.dependencies = deps
                    .iter()
                    .filter_map(|(k, v)| {
                        let v = match v {
                            Value::Array(_) => Some(Dependency::Props(to_strings(v))),
                            _ => Some(Dependency::SchemaRef(enqueue(
                                format!("dependencies/{}", escape(k)),
                                queue,
                            ))),
                        };
                        v.map(|v| (k.clone(), v))
                    })
                    .collect();
            }
        }

        if root.has_vocab("validation") {
            if let Some(t) = obj.get("type") {
                match t {
                    Value::String(t) => s.types.extend(Type::from_str(t)),
                    Value::Array(tt) => {
                        s.types.extend(tt.iter().filter_map(|t| {
                            if let Value::String(t) = t {
                                Type::from_str(t)
                            } else {
                                None
                            }
                        }));
                    }
                    _ => {}
                }
            }

            if let Some(Value::Array(e)) = obj.get("enum") {
                s.enum_ = e.clone();
            }

            s.multiple_of = load_num("multipleOf");

            s.maximum = load_num("maximum");
            if let Some(Value::Bool(exclusive)) = obj.get("exclusiveMaximum") {
                if *exclusive {
                    (s.maximum, s.exclusive_maximum) = (None, s.maximum);
                }
            } else {
                s.exclusive_maximum = load_num("exclusiveMaximum");
            }

            s.minimum = load_num("minimum");
            if let Some(Value::Bool(exclusive)) = obj.get("exclusiveMinimum") {
                if *exclusive {
                    (s.minimum, s.exclusive_minimum) = (None, s.minimum);
                }
            } else {
                s.exclusive_minimum = load_num("exclusiveMinimum");
            }

            s.max_length = load_usize("maxLength");
            s.min_length = load_usize("minLength");

            if let Some(Value::String(p)) = obj.get("pattern") {
                s.pattern = Some(Regex::new(p).map_err(|e| CompileError::Bug(e.into()))?);
            }

            s.max_items = load_usize("maxItems");
            s.min_items = load_usize("minItems");
            if let Some(Value::Bool(unique)) = obj.get("uniqueItems") {
                s.unique_items = *unique;
            }

            s.max_properties = load_usize("maxProperties");
            s.min_properties = load_usize("minProperties");

            if let Some(req) = obj.get("required") {
                s.required = to_strings(req);
            }
        }

        // format --
        if self.assert_format
            || root.has_vocab(if root.draft.version < 2019 {
                "core"
            } else if root.draft.version == 2019 {
                "format"
            } else {
                "format-assertion"
            })
        {
            if let Some(Value::String(format)) = obj.get("format") {
                let func = self
                    .formats
                    .get(format.as_str())
                    .or_else(|| FORMATS.get(format.as_str()));
                if let Some(func) = func {
                    s.format = Some((format.to_owned(), func.clone()));
                }
            }
        }

        // draft6 --
        if root.draft.version >= 6 {
            if root.has_vocab("applicator") {
                s.contains = enqueue_prop("contains", queue);
                s.property_names = enqueue_prop("propertyNames", queue);
            }

            if root.has_vocab("validation") {
                if let Some(constant) = obj.get("const") {
                    s.constant = Some(constant.clone());
                }
            }
        }

        // draft7 --
        if root.draft.version >= 7 {
            if root.has_vocab("applicator") {
                s.if_ = enqueue_prop("if", queue);
                if s.if_.is_some() {
                    s.then = enqueue_prop("then", queue);
                    s.else_ = enqueue_prop("else", queue);
                }
            }
            if self.assert_content {
                if let Some(Value::String(encoding)) = obj.get("contentEncoding") {
                    let func = self
                        .decoders
                        .get(encoding.as_str())
                        .or_else(|| DECODERS.get(encoding.as_str()));
                    if let Some(func) = func {
                        s.content_encoding = Some((encoding.to_owned(), func.clone()));
                    }
                }

                if let Some(Value::String(media_type)) = obj.get("contentMediaType") {
                    let func = self
                        .media_types
                        .get(media_type.as_str())
                        .or_else(|| MEDIA_TYPES.get(media_type.as_str()));
                    if let Some(func) = func {
                        s.content_media_type = Some((media_type.to_owned(), func.clone()));
                    }
                }
            }
        }

        // draft2019 --
        if root.draft.version >= 2019 {
            if root.has_vocab("core") {
                s.recursive_ref = enqueue_ref("$recursiveRef", queue)?;
                if let Some(Value::Bool(b)) = obj.get("$recursiveAnchor") {
                    s.recursive_anchor = *b;
                }
            }

            if root.has_vocab("validation") {
                if s.contains.is_some() {
                    s.max_contains = load_usize("maxContains");
                    s.min_contains = load_usize("minContains");
                }

                if let Some(Value::Object(dep_req)) = obj.get("dependentRequired") {
                    for (pname, pvalue) in dep_req {
                        s.dependent_required
                            .insert(pname.clone(), to_strings(pvalue));
                    }
                }
            }

            if root.has_vocab("applicator") {
                s.dependent_schemas = enqueue_map("dependentSchemas", queue);
            }

            if root.has_vocab(if root.draft.version == 2019 {
                "applicator"
            } else {
                "unevaluated"
            }) {
                s.unevaluated_items = enqueue_prop("unevaluatedItems", queue);
                s.unevaluated_properties = enqueue_prop("unevaluatedProperties", queue);
            }
        }

        // draft2020 --
        if root.draft.version >= 2020 {
            if root.has_vocab("core") {
                s.dynamic_ref = enqueue_ref("$dynamicRef", queue)?;
                if let Some(Value::String(anchor)) = obj.get("$dynamicAnchor") {
                    s.dynamic_anchor = Some(anchor.to_owned());
                }
            }

            if root.has_vocab("applicator") {
                s.prefix_items = enquue_arr("prefixItems", queue);
                s.items2020 = enqueue_prop("items", queue);
            }
        }

        Ok(s)
    }
}

#[derive(Debug)]
pub enum CompileError {
    ParseUrlError {
        url: String,
        src: Box<dyn Error>,
    },
    LoadUrlError {
        url: String,
        src: Box<dyn Error>,
    },
    UnsupportedUrl {
        url: String,
    },
    InvalidMetaSchema {
        url: String,
    },
    MetaSchemaCycle {
        url: String,
    },
    NotValid(ValidationError),
    InvalidId {
        loc: String,
    },
    InvalidAnchor {
        loc: String,
    },
    DuplicateId {
        url: String,
        id: String,
    },
    DuplicateAnchor {
        url: String,
        anchor: String,
    },
    InvalidJsonPointer(String),
    JsonPointerNotFound(String),
    AnchorNotFound {
        schema_url: String,
        anchor_url: String,
    },
    UnsupprtedVocabulary {
        url: String,
        vocabulary: String,
    },
    InvalidRegex {
        url: String,
        regex: String,
    },
    Bug(Box<dyn Error>),
}

impl Error for CompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LoadUrlError { src, .. } => Some(src.as_ref()),
            _ => None,
        }
    }
}

impl Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseUrlError { url, src } => {
                if f.alternate() {
                    write!(f, "error parsing url {url}: {src}")
                } else {
                    write!(f, "error parsing {url}")
                }
            }
            Self::LoadUrlError { url, src } => {
                if f.alternate() {
                    write!(f, "error loading {url}: {src}")
                } else {
                    write!(f, "error loading {url}")
                }
            }
            Self::UnsupportedUrl { url } => write!(f, "loading {url} unsupported"),
            Self::InvalidMetaSchema { url } => write!(f, "invalid $schema in {url}"),
            Self::MetaSchemaCycle { url } => {
                write!(f, "cycle in resolving $schema in {url}")
            }
            Self::NotValid(ve) => {
                if f.alternate() {
                    write!(f, "not valid against metaschema: {ve:#}")
                } else {
                    write!(f, "not valid against metaschema")
                }
            }
            Self::InvalidId { loc } => write!(f, "invalid $id at {loc}"),
            Self::InvalidAnchor { loc } => write!(f, "invalid $anchor at {loc}"),
            Self::DuplicateId { url, id } => write!(f, "duplicate $id {id} in {url}"),
            Self::DuplicateAnchor { url, anchor } => {
                write!(f, "duplicate $anchor {anchor:?} in {url}")
            }
            Self::InvalidJsonPointer(loc) => write!(f, "invalid json-pointer {loc}"),
            Self::JsonPointerNotFound(loc) => write!(f, "json-pointer in {loc} not found"),
            Self::AnchorNotFound {
                schema_url,
                anchor_url,
            } => {
                write!(
                    f,
                    "anchor in {anchor_url} is not found in schema {schema_url}"
                )
            }
            Self::UnsupprtedVocabulary { url, vocabulary } => {
                write!(f, "unsupported vocabulary {vocabulary} in {url}")
            }
            Self::InvalidRegex { url, regex } => {
                write!(f, "invalid regex {} at {}", quote(regex), url)
            }
            Self::Bug(src) => {
                write!(
                    f,
                    "encountered bug in jsonschema compiler. please report: {src}"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compiler() {
        let sch: Value = serde_json::from_str(r#"{"type":"string"}"#).unwrap();
        let mut c = Compiler::default();
        let url = Url::parse("http://a.com/schema.json").unwrap();
        c.roots.or_insert(url.clone(), sch).unwrap();
        let loc = format!("{url}#");
        let mut schemas = Schemas::default();
        let sch_index = c.compile(&mut schemas, loc.clone()).unwrap();
        let inst: Value = Value::String("xx".into());
        schemas.validate(&inst, sch_index).unwrap();
    }

    #[test]
    fn test_debug() {
        run_single(
            Draft::V6,
            r##"
            {"type": "integer"}            
            "##,
            r##"
            1.0
            "##,
            true,
        );
    }

    fn run_single(draft: Draft, schema: &str, data: &str, valid: bool) {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let data: Value = serde_json::from_str(data).unwrap();

        let url = "http://testsuite.com/schema.json";
        let mut schemas = Schemas::default();
        let mut compiler = Compiler::default();
        compiler.set_default_draft(draft);
        compiler.add_resource(url, schema).unwrap();
        let sch_index = compiler.compile(&mut schemas, url.into()).unwrap();
        let result = schemas.validate(&data, sch_index);
        if let Err(e) = &result {
            println!("validation failed: {e:#}");
        }
        assert_eq!(result.is_ok(), valid);
    }
}
