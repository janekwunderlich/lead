#[allow(unused_imports)]
use crate::am::amhandler;
use pgrx::{Internal, IntoDatum, PgList, PgRelation, extension_sql, pg_extern, pg_sys};
use tinql::runtime::{evaluate, lower::lower, subtokenize::sub_tokenize, tokenize_doc};
use tokenizer::presets::default_pipeline;

fn evaluate_text(document: &str, query_text: &str) -> Result<bool, String> {
    let pipeline = default_pipeline();
    let parsed = tinql::parse(query_text, tinql::ImplicitOp::And).map_err(|e| e.to_string())?;
    let analyzed = sub_tokenize(parsed, pipeline).map_err(|e| e.to_string())?;
    let query = lower(&analyzed).map_err(|e| e.to_string())?;
    let document = tokenize_doc(document, pipeline);
    evaluate(&query, &document)
        .map(|result| result.matched)
        .map_err(|e| e.to_string())
}

#[pg_extern(immutable, parallel_safe)]
pub fn tin_text_cmpfunc(document: &str, query: &str) -> bool {
    evaluate_text(document, query)
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"))
}

#[pg_extern(stable, parallel_safe)]
fn text_matches_index(document: &str, query: &str, index_oid: pg_sys::Oid) -> bool {
    let index = unsafe { PgRelation::with_lock(index_oid, pg_sys::AccessShareLock as _) };
    let tin_am = unsafe { pg_sys::get_index_am_oid(c"tin".as_ptr(), false) };
    if unsafe { (*(*index.as_ptr()).rd_rel).relam } != tin_am {
        pgrx::error!("text_matches_index requires a tin index");
    }
    let pipeline = unsafe { crate::options::tokenizer(index.as_ptr()) };
    let query = tinql::runtime::parse_tinql_to_query(query, &pipeline)
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"));
    evaluate(&query, &tokenize_doc(document, &pipeline))
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"))
        .matched
}

fn lookup_index_match() -> pg_sys::Oid {
    let types = [pg_sys::TEXTOID, pg_sys::TEXTOID, pg_sys::OIDOID];
    unsafe {
        let names = pg_sys::stringToQualifiedNameList(
            c"tin.text_matches_index".as_ptr(),
            std::ptr::null_mut(),
        );
        pg_sys::LookupFuncName(names, types.len() as i32, types.as_ptr(), false)
    }
}

/// Read either the original operator or its tokenizer-bound replacement.
pub(crate) unsafe fn search_arguments(
    node: *mut pg_sys::Node,
) -> Option<(*mut pg_sys::Node, *mut pg_sys::Node)> {
    unsafe {
        if node.is_null() {
            return None;
        }
        let args = match (*node).type_ {
            pg_sys::NodeTag::T_OpExpr => {
                let op = &*node.cast::<pg_sys::OpExpr>();
                let name = pg_sys::get_opname(op.opno);
                if name.is_null() || std::ffi::CStr::from_ptr(name).to_bytes() != b"==>" {
                    return None;
                }
                op.args
            }
            pg_sys::NodeTag::T_FuncExpr => {
                let function = &*node.cast::<pg_sys::FuncExpr>();
                if function.funcid != lookup_index_match() {
                    return None;
                }
                function.args
            }
            _ => return None,
        };
        if pg_sys::list_length(args) < 2 {
            return None;
        }
        Some((
            pg_sys::list_nth(args, 0).cast(),
            pg_sys::list_nth(args, 1).cast(),
        ))
    }
}

#[pg_extern(immutable, parallel_unsafe)]
fn match_support(request: Internal) -> Internal {
    let unhandled = || Internal::from(Some(pg_sys::Datum::from(0_usize)));
    let Some(datum) = request.into_datum() else {
        return unhandled();
    };
    unsafe {
        let node = datum.cast_mut_ptr::<pg_sys::Node>();
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestSimplify {
            return unhandled();
        }
        let request = &*node.cast::<pg_sys::SupportRequestSimplify>();
        if request.root.is_null() || request.fcall.is_null() {
            return unhandled();
        }
        let args = (*request.fcall).args;
        let document = pg_sys::list_nth(args, 0).cast::<pg_sys::Node>();
        let vars = pg_sys::pull_varnos(request.root, document);
        let mut varno = 0;
        if !pg_sys::bms_get_singleton_member(vars, &mut varno) {
            return unhandled();
        }
        let parse = (*request.root).parse;
        if varno <= 0 || varno > pg_sys::list_length((*parse).rtable) {
            return unhandled();
        }
        let rte = &*pg_sys::list_nth((*parse).rtable, varno - 1).cast::<pg_sys::RangeTblEntry>();
        if rte.rtekind != pg_sys::RTEKind::RTE_RELATION {
            return unhandled();
        }
        let Some(index) = crate::score::find_matching_tin_index(
            rte.relid,
            varno,
            document,
            (*(*parse).jointree).quals.cast(),
        ) else {
            return unhandled();
        };
        let mut bound_args = PgList::<pg_sys::Node>::new();
        bound_args.push(pg_sys::copyObjectImpl(document.cast()).cast());
        bound_args.push(pg_sys::copyObjectImpl(pg_sys::list_nth(args, 1).cast()).cast());
        bound_args.push(
            pg_sys::makeConst(
                pg_sys::OIDOID,
                -1,
                pg_sys::InvalidOid,
                4,
                index.into_datum().unwrap(),
                false,
                true,
            )
            .cast(),
        );
        let replacement = pg_sys::makeFuncExpr(
            lookup_index_match(),
            pg_sys::BOOLOID,
            bound_args.into_pg(),
            pg_sys::InvalidOid,
            (*request.fcall).inputcollid,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
        );
        Internal::from(Some(pg_sys::Datum::from(replacement as usize)))
    }
}

#[pg_extern(immutable, parallel_unsafe)]
fn index_match_support(request: Internal) -> Internal {
    let unhandled = || Internal::from(Some(pg_sys::Datum::from(0_usize)));
    let Some(datum) = request.into_datum() else {
        return unhandled();
    };
    unsafe {
        let node = datum.cast_mut_ptr::<pg_sys::Node>();
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestIndexCondition {
            return unhandled();
        }
        let request = &mut *node.cast::<pg_sys::SupportRequestIndexCondition>();
        if request.indexarg != 0
            || request.indexcol != 0
            || (*request.node).type_ != pg_sys::NodeTag::T_FuncExpr
        {
            return unhandled();
        }
        let function = &*request.node.cast::<pg_sys::FuncExpr>();
        let index_node = pg_sys::list_nth(function.args, 2).cast::<pg_sys::Node>();
        if (*index_node).type_ != pg_sys::NodeTag::T_Const {
            return unhandled();
        }
        let index = &*index_node.cast::<pg_sys::Const>();
        if index.constisnull
            || index.constvalue.value() as u32 != (*request.index).indexoid.to_u32()
        {
            return unhandled();
        }
        let query = pg_sys::list_nth(function.args, 1).cast::<pg_sys::Node>();
        if !pg_sys::is_pseudo_constant_for_index(request.root, query, request.index) {
            return unhandled();
        }
        let operator =
            pg_sys::get_opfamily_member(request.opfamily, pg_sys::TEXTOID, pg_sys::TEXTOID, 1);
        if operator == pg_sys::InvalidOid {
            return unhandled();
        }
        let clause = pg_sys::make_opclause(
            operator,
            pg_sys::BOOLOID,
            false,
            pg_sys::copyObjectImpl(pg_sys::list_nth(function.args, 0).cast()).cast(),
            pg_sys::copyObjectImpl(query.cast()).cast(),
            pg_sys::InvalidOid,
            function.inputcollid,
        );
        // Lead returns whole heap pages. Recheck using the tokenizer-bound function.
        request.lossy = true;
        let mut clauses = PgList::<pg_sys::Expr>::new();
        clauses.push(clause);
        Internal::from(Some(pg_sys::Datum::from(clauses.into_pg() as usize)))
    }
}

extension_sql!(
    r#"
ALTER FUNCTION @extschema@.tin_text_cmpfunc(pg_catalog.text, pg_catalog.text) SUPPORT @extschema@.match_support;
ALTER FUNCTION @extschema@.text_matches_index(pg_catalog.text, pg_catalog.text, pg_catalog.oid) SUPPORT @extschema@.index_match_support;
"#,
    name = "match_support_bindings",
    requires = [
        tin_text_cmpfunc,
        text_matches_index,
        match_support,
        index_match_support
    ]
);

extension_sql!(
    r#"
CREATE OPERATOR pg_catalog.==> (
    PROCEDURE = @extschema@.tin_text_cmpfunc,
    LEFTARG = pg_catalog.text,
    RIGHTARG = pg_catalog.text
);

CREATE OPERATOR CLASS @extschema@.tin_text_ops DEFAULT FOR TYPE pg_catalog.text USING tin AS
    OPERATOR 1 pg_catalog.==>(pg_catalog.text, pg_catalog.text),
    STORAGE pg_catalog.text;
"#,
    name = "tin_text_operator",
    requires = [amhandler, tin_text_cmpfunc]
);

#[cfg(test)]
mod tests {
    use super::evaluate_text;

    #[test]
    fn boolean_and_positional_queries_are_exact() {
        assert!(evaluate_text("A craft beer bar", "craft AND beer").unwrap());
        assert!(evaluate_text("A craft beer bar", "\"craft beer\"").unwrap());
        assert!(!evaluate_text("Beer for craft fans", "\"craft beer\"").unwrap());
    }

    #[test]
    fn expansions_use_the_document_term_universe() {
        assert!(evaluate_text("brewhouse", "brew*").unwrap());
        assert!(evaluate_text("jalapeno", "jalapeño~1").unwrap());
        assert!(!evaluate_text("winery", "brew*").unwrap());
    }

    #[test]
    fn empty_documents_do_not_match_match_all() {
        assert!(!evaluate_text("...", "*").unwrap());
    }

    #[test]
    fn invalid_queries_are_reported() {
        assert!(evaluate_text("beer", "beer OR").is_err());
    }
}
