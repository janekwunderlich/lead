use super::*;
use pgrx::PgBox;
use rustc_hash::FxHashSet;
use tinql::runtime::{evaluate, tokenize_doc};

struct MultiCorpus {
    keys: Vec<(u32, Option<CacheKey>)>,
    condition: Vec<i32>,
    by_document: FxHashMap<Vec<Option<String>>, f32>,
    max: f32,
}

thread_local! {
    static CACHE: RefCell<Option<MultiCorpus>> = const { RefCell::new(None) };
}

#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL signature used by scoring support"
)]
fn score_many_bound(
    documents: Vec<Option<String>>,
    queries: Vec<Option<String>>,
    heap_oid: i32,
    indexes: Vec<i32>,
    mode: i32,
    condition: Vec<i32>,
    dense_ratio: Option<f32>,
    k1: Option<f32>,
    b: Option<f32>,
    term_add: Option<Vec<String>>,
    term_replace: Option<Vec<String>>,
) -> f32 {
    if documents.len() != indexes.len() || queries.len() != indexes.len() {
        pgrx::error!("mismatched cross-column scoring inputs");
    }
    let keys = indexes
        .iter()
        .zip(queries)
        .map(|(&index, query)| {
            (
                index as u32,
                query.map(|query| CacheKey {
                    transaction: unsafe { pg_sys::GetTopTransactionIdIfAny().into_inner() },
                    command: unsafe { pg_sys::GetCurrentCommandId(false) },
                    heap_oid: heap_oid as u32,
                    index_oid: index as u32,
                    query,
                    full: mode == 1 || mode == 3,
                    dense: dense_ratio.unwrap_or(DenseRatio::DEFAULT).to_bits(),
                    k1: bits(k1),
                    b: bits(b),
                    add: term_add.clone(),
                    replace: term_replace.clone(),
                }),
            )
        })
        .collect::<Vec<_>>();
    CACHE.with_borrow_mut(|slot| {
        if slot
            .as_ref()
            .is_none_or(|cached| cached.keys != keys || cached.condition != condition)
        {
            let fields = build_fields(&keys);
            let heap_oid = pg_sys::Oid::from(heap_oid as u32);
            let definitions = indexes
                .iter()
                .map(|&index| index_definition(heap_oid, pg_sys::Oid::from(index as u32)))
                .collect::<Vec<_>>();
            let expressions = definitions
                .iter()
                .map(|(expression, _)| format!("({expression})::text"))
                .collect::<Vec<_>>()
                .join(", ");
            let predicates = definitions
                .iter()
                .filter_map(|(_, predicate)| predicate.as_ref())
                .map(|predicate| format!("({predicate})"))
                .collect::<Vec<_>>();
            let filter = if predicates.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", predicates.join(" AND "))
            };
            let sql = format!(
                "SELECT ARRAY[{expressions}] FROM {}{filter}",
                qualified_relation(heap_oid)
            );
            let rows = Spi::connect(|client| {
                client
                    .select(&sql, None, &[])
                    .unwrap_or_else(|error| pgrx::error!("tin score corpus scan failed: {error}"))
                    .map(|row| row.get::<Vec<Option<String>>>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            let mut by_document = FxHashMap::default();
            let mut max = 0.0_f32;
            for row in rows {
                pgrx::check_for_interrupts!();
                let matches = row
                    .iter()
                    .zip(&fields.matches)
                    .map(|(document, field)| {
                        document
                            .as_ref()
                            .zip(field.as_ref())
                            .map(|(document, field)| field.contains(document))
                    })
                    .collect::<Vec<_>>();
                let score = sum_scores_in_order(fields.scored.iter().map(|(slot, field)| {
                    row[*slot]
                        .as_ref()
                        .and_then(|document| field.by_document.get(document))
                        .copied()
                        .unwrap_or(0.0)
                }));
                if qualifies(&condition, &matches) {
                    max = max.max(score);
                }
                by_document.insert(row, score);
            }
            *slot = Some(MultiCorpus {
                keys,
                condition,
                by_document,
                max,
            });
        }
        let corpus = slot.as_ref().expect("cross-column corpus was populated");
        if mode == 2 || mode == 3 {
            corpus.max
        } else {
            corpus.by_document.get(&documents).copied().unwrap_or(0.0)
        }
    })
}

struct FieldCorpora {
    matches: Vec<Option<FxHashSet<String>>>,
    scored: Vec<(usize, ScoreCorpus)>,
}

fn build_fields(keys: &[(u32, Option<CacheKey>)]) -> FieldCorpora {
    let mut groups = std::collections::BTreeMap::<u32, Vec<usize>>::new();
    for (slot, (index, key)) in keys.iter().enumerate() {
        if key.is_some() {
            groups.entry(*index).or_default().push(slot);
        }
    }
    let mut fields = FieldCorpora {
        matches: vec![None; keys.len()],
        scored: Vec::new(),
    };
    for (index_oid, slots) in groups {
        let mut key = keys[slots[0]].1.as_ref().unwrap().clone();
        key.query = slots
            .iter()
            .map(|&slot| format!("({})", keys[slot].1.as_ref().unwrap().query))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut corpus = build_corpus(
            key.clone(),
            key.k1.map(f32::from_bits),
            key.b.map(f32::from_bits),
            key.add.clone(),
            key.replace.clone(),
        );
        let index = unsafe {
            PgRelation::with_lock(pg_sys::Oid::from(index_oid), pg_sys::AccessShareLock as _)
        };
        let tokenizer = unsafe { crate::options::tokenizer(index.as_ptr()) };
        let queries = slots
            .iter()
            .map(|&slot| {
                fields.matches[slot] = Some(FxHashSet::default());
                parse_tinql_to_query(&keys[slot].1.as_ref().unwrap().query, &tokenizer)
                    .unwrap_or_else(|error| pgrx::error!("TIN score query error: {error}"))
            })
            .collect::<Vec<_>>();
        for document in corpus.by_document.keys() {
            pgrx::check_for_interrupts!();
            let tokenized = tokenize_doc(document, &tokenizer);
            for (&slot, query) in slots.iter().zip(&queries) {
                if evaluate(query, &tokenized)
                    .unwrap_or_else(|error| {
                        pgrx::error!("TIN score query evaluation failed: {error}")
                    })
                    .matched
                {
                    fields.matches[slot]
                        .as_mut()
                        .unwrap()
                        .insert(document.clone());
                }
            }
        }
        corpus.by_document.retain(|document, _| {
            slots
                .iter()
                .any(|&slot| fields.matches[slot].as_ref().unwrap().contains(document))
        });
        fields.scored.push((slots[0], corpus));
    }
    fields
}

// Postfix Boolean expression over search predicates. Preserve SQL NULL semantics.
const AND: i32 = -1;
const OR: i32 = -2;
const NOT: i32 = -3;
const TRUE: i32 = -4;

fn qualifies(condition: &[i32], matches: &[Option<bool>]) -> bool {
    let mut stack = Vec::new();
    for &op in condition {
        match op {
            TRUE => stack.push(Some(true)),
            NOT => {
                let value = stack.pop().expect("NOT operand");
                stack.push(value.map(|v| !v));
            }
            AND | OR => {
                let right = stack.pop().expect("right operand");
                let left = stack.pop().expect("left operand");
                stack.push(if op == AND {
                    match (left, right) {
                        (Some(false), _) | (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    }
                } else {
                    match (left, right) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                });
            }
            slot => stack.push(matches[slot as usize]),
        }
    }
    stack.pop() == Some(Some(true))
}

struct Field {
    document: *mut pg_sys::Node,
    query: *mut pg_sys::Node,
    index: pg_sys::Oid,
}

unsafe fn condition(node: *mut pg_sys::Node, fields: &[Field], out: &mut Vec<i32>) -> bool {
    unsafe {
        if node.is_null() {
            return false;
        }
        if (*node).type_ == pg_sys::NodeTag::T_BoolExpr || (*node).type_ == pg_sys::NodeTag::T_List
        {
            let (args, op) = if (*node).type_ == pg_sys::NodeTag::T_List {
                (node.cast(), AND)
            } else {
                let boolean = &*node.cast::<pg_sys::BoolExpr>();
                (
                    boolean.args,
                    match boolean.boolop {
                        pg_sys::BoolExprType::AND_EXPR => AND,
                        pg_sys::BoolExprType::OR_EXPR => OR,
                        _ => NOT,
                    },
                )
            };
            let mut count = 0;
            for i in 0..pg_sys::list_length(args) {
                if condition(pg_sys::list_nth(args, i).cast(), fields, out) {
                    if count > 0 || op == NOT {
                        out.push(op);
                    }
                    count += 1;
                }
            }
            return count > 0;
        }
        let mut binding = QualBinding {
            matches: Vec::new(),
        };
        find_qual(node, (&mut binding as *mut QualBinding).cast());
        if binding.matches.len() == 1 {
            let (document, query) = binding.matches[0];
            if let Some(slot) = fields.iter().position(|field| {
                pg_sys::equal(document.cast(), field.document.cast())
                    && pg_sys::equal(query.cast(), field.query.cast())
            }) {
                out.push(slot as i32);
                return true;
            }
        }
        // Other SQL filters do not contribute terms to this relation's TIN scan.
        false
    }
}

unsafe fn array(
    values: impl IntoIterator<Item = *mut pg_sys::Node>,
    element: pg_sys::Oid,
    array_type: pg_sys::Oid,
) -> *mut pg_sys::Node {
    unsafe {
        let mut expression = PgBox::<pg_sys::ArrayExpr>::alloc_node(pg_sys::NodeTag::T_ArrayExpr);
        expression.array_typeid = array_type;
        expression.element_typeid = element;
        let mut elements = PgList::<pg_sys::Node>::new();
        for value in values {
            elements.push(pg_sys::copyObjectImpl(value.cast()).cast());
        }
        expression.elements = elements.into_pg();
        expression.into_pg().cast()
    }
}

fn lookup() -> pg_sys::Oid {
    let types = [
        pg_sys::TEXTARRAYOID,
        pg_sys::TEXTARRAYOID,
        pg_sys::INT4OID,
        pg_sys::INT4ARRAYOID,
        pg_sys::INT4OID,
        pg_sys::INT4ARRAYOID,
        pg_sys::FLOAT4OID,
        pg_sys::FLOAT4OID,
        pg_sys::FLOAT4OID,
        pg_sys::TEXTARRAYOID,
        pg_sys::TEXTARRAYOID,
    ];
    unsafe {
        let names = pg_sys::stringToQualifiedNameList(
            c"tin.score_many_bound".as_ptr(),
            std::ptr::null_mut(),
        );
        pg_sys::LookupFuncName(names, types.len() as i32, types.as_ptr(), false)
    }
}

pub(super) unsafe fn is_full_score(
    function: &pg_sys::FuncExpr,
    binding: &FullScoreBinding,
) -> bool {
    unsafe {
        if function.funcid != lookup() {
            return false;
        }
        let mode = pg_sys::list_nth(function.args, 4).cast::<pg_sys::Const>();
        let documents = pg_sys::list_nth(function.args, 0).cast::<pg_sys::ArrayExpr>();
        if (*mode).xpr.type_ != pg_sys::NodeTag::T_Const
            || (*documents).xpr.type_ != pg_sys::NodeTag::T_ArrayExpr
        {
            return false;
        }
        let mode = &*mode;
        let documents = &*documents;
        mode.constvalue.value() == 1
            && PgList::<pg_sys::Node>::from_pg(documents.elements)
                .iter_ptr()
                .any(|doc| pg_sys::equal(doc.cast(), binding.document.cast()))
    }
}

pub(super) unsafe fn rewrite(
    request: &pg_sys::SupportRequestSimplify,
    ctid: &pg_sys::Var,
    heap: pg_sys::Oid,
    matches: &[(*mut pg_sys::Node, *mut pg_sys::Node)],
) -> Option<*mut pg_sys::Node> {
    unsafe {
        let mut fields = matches
            .iter()
            .filter_map(|&(document, query)| {
                find_matching_tin_index(
                    heap,
                    ctid.varno,
                    document,
                    (*(*(*request.root).parse).jointree).quals.cast(),
                )
                .map(|index| Field {
                    document,
                    query,
                    index,
                })
            })
            .collect::<Vec<_>>();
        if fields.len() < 2 || fields.iter().all(|field| field.index == fields[0].index) {
            return None;
        }
        fields.sort_by_cached_key(|field| {
            (
                field.index.to_u32(),
                CStr::from_ptr(pg_sys::nodeToString(field.query.cast()))
                    .to_bytes()
                    .to_vec(),
            )
        });
        fields.dedup_by(|a, b| a.index == b.index && pg_sys::equal(a.query.cast(), b.query.cast()));
        let parse = (*request.root).parse;
        let mut program = Vec::new();
        if !condition((*(*parse).jointree).quals.cast(), &fields, &mut program) {
            program.push(TRUE);
        }
        let name = CStr::from_ptr(pg_sys::get_func_name((*request.fcall).funcid));
        let mode = if name.to_bytes() == b"full_score" {
            1
        } else if name.to_bytes() == b"max_score" {
            let mut binding = FullScoreBinding {
                ctid,
                document: fields[0].document,
                support: pg_sys::get_func_support((*request.fcall).funcid),
                bound: lookup_score_bound(),
            };
            if pg_sys::query_tree_walker(
                parse,
                Some(has_full_score),
                (&mut binding as *mut FullScoreBinding).cast(),
                pg_sys::QTW_IGNORE_RC_SUBQUERIES as i32,
            ) {
                3
            } else {
                2
            }
        } else {
            0
        };
        let mut args = PgList::<pg_sys::Node>::new();
        args.push(array(
            fields.iter().map(|f| f.document),
            pg_sys::TEXTOID,
            pg_sys::TEXTARRAYOID,
        ));
        args.push(array(
            fields.iter().map(|f| f.query),
            pg_sys::TEXTOID,
            pg_sys::TEXTARRAYOID,
        ));
        args.push(make_int4_const(heap.to_u32() as i32).cast());
        args.push(array(
            fields
                .iter()
                .map(|f| make_int4_const(f.index.to_u32() as i32).cast()),
            pg_sys::INT4OID,
            pg_sys::INT4ARRAYOID,
        ));
        args.push(make_int4_const(mode).cast());
        args.push(array(
            program.iter().map(|&op| make_int4_const(op).cast()),
            pg_sys::INT4OID,
            pg_sys::INT4ARRAYOID,
        ));
        let nargs = pg_sys::list_length((*request.fcall).args);
        if mode == 0 {
            for i in 1..=5 {
                args.push(
                    pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, i).cast())
                        .cast(),
                );
            }
        } else {
            args.push(make_null_const(pg_sys::FLOAT4OID).cast());
            for i in 1..=2 {
                args.push(if mode == 1 && nargs == 3 {
                    pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, i).cast()).cast()
                } else {
                    make_null_const(pg_sys::FLOAT4OID).cast()
                });
            }
            args.push(make_null_const(pg_sys::TEXTARRAYOID).cast());
            args.push(make_null_const(pg_sys::TEXTARRAYOID).cast());
        }
        Some(
            pg_sys::makeFuncExpr(
                lookup(),
                pg_sys::FLOAT4OID,
                args.into_pg(),
                pg_sys::InvalidOid,
                pg_sys::InvalidOid,
                pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
            )
            .cast(),
        )
    }
}

pgrx::extension_sql!(
    "REVOKE ALL ON FUNCTION @extschema@.score_many_bound(text[], text[], int4, int4[], int4, int4[], real, real, real, text[], text[]) FROM PUBLIC;",
    name = "score_many_binding",
    requires = [score_many_bound]
);
