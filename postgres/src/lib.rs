use pgrx::pg_guard;

::pgrx::pg_module_magic!(name);

mod am;
mod bm25;
mod highlight;
mod highlight_udfs;
mod match_positions;
mod operator;
pub(crate) mod options;
mod score;
mod tf_bucket;
mod udfs;

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::init();
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries=''"]
    }
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::Json;
    use pgrx::prelude::*;

    #[pg_test]
    fn bitmap_index_rechecks_heap_pages_without_preloading() {
        assert_eq!(
            Spi::get_one::<String>("SHOW shared_preload_libraries").unwrap(),
            Some(String::new())
        );
        Spi::run("CREATE TABLE lite_search (id int, body text)").unwrap();
        Spi::run(
            "INSERT INTO lite_search VALUES
               (1, 'craft beer'), (2, 'wine'), (3, 'beer festival')",
        )
        .unwrap();
        Spi::run("CREATE INDEX lite_search_idx ON lite_search USING tin (body)").unwrap();
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        let ids = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY id) FROM lite_search WHERE body ==> 'beer'",
        )
        .unwrap();
        assert_eq!(ids, Some(vec![1, 3]));
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (ANALYZE, FORMAT JSON)
             SELECT id FROM lite_search WHERE body ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 1);
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Index Name"], "lite_search_idx");
    }

    #[pg_test]
    fn bitmap_scan_follows_heap_growth_and_truncate() {
        Spi::run(
            "CREATE TABLE lite_growth (id int, body text);
             CREATE INDEX lite_growth_idx ON lite_growth USING tin (body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(0)
        );
        Spi::run(
            "INSERT INTO lite_growth
               SELECT n, CASE WHEN n % 50 = 0 THEN 'beer' ELSE 'wine' END
                         || repeat(' filler', 80)
               FROM generate_series(1, 400) AS n;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(8)
        );
        Spi::run(
            "TRUNCATE lite_growth;
             INSERT INTO lite_growth VALUES (1, 'beer'), (2, 'wine');",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_growth WHERE body ==> 'beer'").unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn bitmap_scan_rechecks_partial_index_predicates_and_expressions() {
        Spi::run(
            "CREATE TABLE lite_partial (id int, body text, active boolean);
             INSERT INTO lite_partial VALUES
               (1, 'BEER', true), (2, 'wine', true),
               (3, 'BEER', false), (4, NULL, true);
             CREATE INDEX lite_partial_idx ON lite_partial
               USING tin (lower(body)) WHERE active;
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (FORMAT JSON)
             SELECT id FROM lite_partial WHERE active AND lower(body) ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(
            plan[0]["Plan"]["Plans"][0]["Index Name"],
            "lite_partial_idx"
        );
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_partial
                 WHERE active AND lower(body) ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1])
        );
        Spi::run("UPDATE lite_partial SET active = true WHERE id = 3").unwrap();
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_partial
                 WHERE active AND lower(body) ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1, 3])
        );
    }

    #[pg_test]
    fn bitmap_union_rechecks_both_search_predicates() {
        Spi::run(
            "CREATE TABLE lite_union (id int, title text, body text);
             INSERT INTO lite_union VALUES
               (1, 'beer', 'wine'), (2, 'wine', 'beer'),
               (3, 'beer', 'beer'), (4, 'wine', 'wine');
             CREATE INDEX lite_union_title_idx ON lite_union USING tin (title);
             CREATE INDEX lite_union_body_idx ON lite_union USING tin (body);
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        let plan = Spi::get_one::<Json>(
            "EXPLAIN (FORMAT JSON)
             SELECT id FROM lite_union WHERE title ==> 'beer' OR body ==> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Node Type"], "BitmapOr");
        assert_eq!(
            Spi::get_one::<Vec<i32>>(
                "SELECT array_agg(id ORDER BY id) FROM lite_union
                 WHERE title ==> 'beer' OR body ==> 'beer'"
            )
            .unwrap(),
            Some(vec![1, 2, 3])
        );
    }

    #[pg_test]
    fn heap_mvcc_owns_updates_and_deletes() {
        Spi::run(
            "CREATE TABLE lite_mvcc (id int, body text);
             INSERT INTO lite_mvcc VALUES (1, 'old term'), (2, 'keep term');
             CREATE INDEX lite_mvcc_idx ON lite_mvcc USING tin (body);
             UPDATE lite_mvcc SET body = 'new term' WHERE id = 1;
             DELETE FROM lite_mvcc WHERE id = 2;
             SET LOCAL enable_seqscan = off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'old'").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'new'").unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM lite_mvcc WHERE body ==> 'keep'").unwrap(),
            Some(0)
        );
        Spi::run("UPDATE lite_mvcc SET id = 3 WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<Vec<i32>>("SELECT array_agg(id) FROM lite_mvcc WHERE body ==> 'new'")
                .unwrap(),
            Some(vec![3])
        );
    }

    #[pg_test]
    fn scoring_rewrite_orders_matching_rows() {
        Spi::run(
            "CREATE TABLE lite_score (id int, body text);
             INSERT INTO lite_score VALUES
               (1, 'rare'), (2, 'rare rare rare'), (3, 'common');
             CREATE INDEX lite_score_idx ON lite_score USING tin (body);",
        )
        .unwrap();
        let ids = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY tin.full_score(ctid) DESC, id)
             FROM lite_score WHERE body ==> 'rare'",
        )
        .unwrap();
        assert_eq!(ids, Some(vec![2, 1]));
    }

    #[pg_test]
    fn scoring_helpers_share_the_same_policy() {
        Spi::run(
            "CREATE TABLE lite_score_helpers (id int, body text);
             INSERT INTO lite_score_helpers VALUES
               (1, 'common rare'), (2, 'common'), (3, 'common');
             CREATE INDEX lite_score_helpers_idx ON lite_score_helpers USING tin (body)",
        )
        .unwrap();
        let full_max = Spi::get_one::<f32>(
            "SELECT max(tin.full_score(ctid))
             FROM lite_score_helpers WHERE body ==> 'rare^1.0'",
        )
        .unwrap()
        .unwrap();
        let reported = Spi::get_one::<f32>(
            "SELECT tin.max_score(ctid)
             FROM lite_score_helpers WHERE body ==> 'rare^1.0' LIMIT 1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(reported, full_max);
        let inspected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(term ORDER BY term)
             FROM tin.score_inspect('lite_score_helpers_idx', 'common OR rare', 0.5)",
        )
        .unwrap();
        assert_eq!(inspected, Some(vec!["rare".to_owned()]));
    }

    #[pg_test]
    fn cross_column_scores_include_each_matching_field() {
        Spi::run(
            "CREATE TABLE lite_fields (id int, title text, body text);
             INSERT INTO lite_fields VALUES (1, 'beer', 'water'), (2, 'water', 'beer'),
               (3, 'beer', 'beer'), (4, NULL, 'beer'), (5, 'beer', NULL);
             CREATE INDEX lite_fields_title ON lite_fields USING tin (title);
             CREATE INDEX lite_fields_body ON lite_fields USING tin (body);",
        )
        .unwrap();
        for scorer in [
            "tin.full_score(ctid)",
            "tin.score(ctid, dense_ratio => 1.1)",
        ] {
            let mut previous = None;
            for predicate in [
                "title ==> 'beer' OR body ==> 'beer'",
                "body ==> 'beer' OR title ==> 'beer'",
            ] {
                let sql = format!(
                    "SELECT array_agg({scorer} ORDER BY id) FROM lite_fields WHERE {predicate}"
                );
                let scores = Spi::get_one::<Vec<f32>>(&sql).unwrap().unwrap();
                assert_eq!(scores.len(), 5);
                assert!(scores[0] > 0.0);
                assert_eq!(scores[0], scores[1]);
                assert_eq!(scores[2], scores[0] + scores[1]);
                assert_eq!(scores[3], scores[0]);
                assert_eq!(scores[4], scores[0]);
                if let Some(previous) = previous {
                    assert_eq!(scores, previous);
                }
                previous = Some(scores);
            }
        }
        let boosted = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(tin.full_score(ctid) ORDER BY id) FROM lite_fields
             WHERE title ==> 'beer^2' OR body ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(boosted[0], 2.0 * boosted[1]);
        assert_eq!(boosted[2], boosted[0] + boosted[1]);
    }

    #[pg_test]
    fn cross_column_maximum_uses_combined_scores_of_matching_rows() {
        Spi::run(
            "CREATE TABLE lite_fields_max (id int, title text, body text);
             INSERT INTO lite_fields_max VALUES
               (1, 'beer beer beer', 'water'), (2, 'water', 'beer beer beer'),
               (3, 'beer' || repeat(' filler', 20), 'beer' || repeat(' filler', 20));
             CREATE INDEX lite_fields_max_title ON lite_fields_max USING tin (title);
             CREATE INDEX lite_fields_max_body ON lite_fields_max USING tin (body);",
        )
        .unwrap();
        for (op, count) in [("OR", 3), ("AND", 1), ("AND NOT", 1)] {
            let sql = format!(
                "SELECT tin.full_score(ctid), tin.max_score(ctid) FROM lite_fields_max
                 WHERE title ==> 'beer' {op} body ==> 'beer'"
            );
            let rows = Spi::connect(|client| {
                client
                    .select(&sql, None, &[])
                    .unwrap()
                    .map(|row| {
                        (
                            row.get::<f32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            assert_eq!(rows.len(), count);
            let maximum = rows.iter().map(|row| row.0).fold(0.0_f32, f32::max);
            assert!(maximum > 0.0);
            for (_, reported) in rows {
                assert_eq!(reported, maximum, "{op}");
            }
        }
    }

    #[pg_test]
    fn cross_column_queries_support_parameters_and_term_edits() {
        Spi::run(
            "CREATE TABLE lite_fields_params (id int, title text, body text);
             INSERT INTO lite_fields_params VALUES (1, 'beer wine', 'cider'), (2, 'beer', 'water'), (3, 'water', 'wine');
             CREATE INDEX lite_fields_params_title ON lite_fields_params USING tin (title);
             CREATE INDEX lite_fields_params_body ON lite_fields_params USING tin (body);"
        ).unwrap();
        let title = Spi::get_one::<f32>(
            "SELECT tin.score(ctid, dense_ratio => 1.1, term_add => ARRAY['wine'])
            FROM lite_fields_params WHERE id = 1 AND title ==> 'beer OR wine'",
        )
        .unwrap()
        .unwrap();
        let body = Spi::get_one::<f32>(
            "SELECT tin.score(ctid, dense_ratio => 1.1, term_add => ARRAY['wine'])
            FROM lite_fields_params WHERE id = 1 AND body ==> 'cider'",
        )
        .unwrap()
        .unwrap();
        let combined = Spi::get_one::<f32>("SELECT tin.score(ctid, dense_ratio => 1.1, term_add => ARRAY['wine'])
            FROM lite_fields_params WHERE id = 1 AND (title ==> 'beer' OR title ==> 'wine' OR body ==> 'cider')").unwrap().unwrap();
        assert_eq!(combined, title + body);
        Spi::run("SET LOCAL plan_cache_mode = force_generic_plan;
            PREPARE lite_fields_query(text, text) AS
            SELECT array_agg(tin.full_score(ctid) ORDER BY id), array_agg(tin.max_score(ctid) ORDER BY id)
            FROM lite_fields_params WHERE title ==> $1 OR body ==> $2;").unwrap();
        for (args, count) in [
            ("'beer', 'cider'", 2),
            ("'missing', 'wine'", 1),
            ("NULL, 'cider'", 1),
        ] {
            let (scores, maxima) =
                Spi::get_two::<Vec<f32>, Vec<f32>>(&format!("EXECUTE lite_fields_query({args})"))
                    .unwrap();
            let scores = scores.unwrap();
            assert_eq!(scores.len(), count);
            assert!(scores.iter().all(|&score| score > 0.0));
            let maximum = scores.iter().copied().fold(0.0_f32, f32::max);
            assert_eq!(maxima.unwrap(), vec![maximum; count]);
        }
        Spi::run("DEALLOCATE lite_fields_query").unwrap();
    }

    #[pg_test]
    fn cross_column_scores_bind_partial_expressions_and_join_aliases() {
        Spi::run(
            "CREATE TABLE lite_fields_expr (id int, title text, body text, active boolean);
             INSERT INTO lite_fields_expr VALUES (1, 'BEER', 'water', true), (2, 'water', 'BEER', true),
               (3, 'BEER', 'BEER', false);
             CREATE INDEX lite_fields_expr_title ON lite_fields_expr USING tin (lower(title)) WHERE active;
             CREATE INDEX lite_fields_expr_body ON lite_fields_expr USING tin (lower(body)) WHERE active;"
        ).unwrap();
        let (score, maximum) = Spi::get_two::<f32, f32>(
            "SELECT tin.full_score(ctid), tin.max_score(ctid) FROM lite_fields_expr
             WHERE active AND (lower(title) ==> 'beer' OR lower(body) ==> 'beer')",
        )
        .unwrap();
        assert!((score.unwrap() - std::f32::consts::LN_2).abs() < 0.000001);
        assert_eq!(score, maximum);
        let (a, b) = Spi::get_two::<f32, f32>(
            "SELECT tin.full_score(a.ctid), tin.full_score(b.ctid)
             FROM lite_fields_expr a JOIN lite_fields_expr b ON true
             WHERE a.id = 1 AND b.id = 2 AND a.active AND b.active
               AND (lower(a.title) ==> 'beer' OR lower(a.body) ==> 'beer')
               AND (lower(b.title) ==> 'water' OR lower(b.body) ==> 'water')",
        )
        .unwrap();
        assert_eq!(a, score);
        assert_eq!(b, score);
    }

    #[pg_test]
    fn full_score_normalization_matches_tin() {
        Spi::run(
            "CREATE TABLE lite_normalization (id int, body text);
             INSERT INTO lite_normalization VALUES
               (1, 'I love fuji apples and juicy mangoes'),
               (2, 'Grape tasting notes from the orchard'),
               (3, 'The best juicy fuji apple in town');
             CREATE INDEX lite_normalization_idx ON lite_normalization USING tin (body)",
        )
        .unwrap();
        for expression in [
            "tin.full_score(ctid) / tin.max_score(ctid)",
            "1::real / tin.max_score(ctid) * tin.full_score(ctid)",
        ] {
            let sql = format!(
                "SELECT {expression} FROM lite_normalization
                 WHERE body ==> 'apple OR grape' AND tin.max_score(ctid) > 0 ORDER BY id"
            );
            let scores = Spi::connect(|client| {
                client
                    .select(&sql, None, &[])
                    .unwrap()
                    .map(|row| row.get::<f32>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(scores.len(), 2);
            assert!((scores[0] - 1.0).abs() < 0.000001);
            assert!((scores[1] - 0.9398665).abs() < 0.000001);
        }
    }

    #[pg_test]
    fn scoring_binds_to_expression_indexes() {
        Spi::run(
            "CREATE TABLE lite_expression_score (id int, s1 text, s2 text);
             INSERT INTO lite_expression_score VALUES
               (1, 'hello', 'world 10'),
               (2, 'hello hello', 'world 10'),
               (3, 'unrelated', 'document');
             INSERT INTO lite_expression_score
               SELECT n, 'noise', n::text FROM generate_series(4, 30) AS n;
             CREATE INDEX lite_expression_score_idx ON lite_expression_score
               USING tin (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT id, tin.score(ctid) AS score
                     FROM lite_expression_score
                     WHERE (s1 || ' ' || s2) ==> 'hello world 10'
                     ORDER BY score DESC, id LIMIT 5",
                    None,
                    &[],
                )
                .unwrap()
                .map(|row| {
                    (
                        row.get::<i32>(1).unwrap().unwrap(),
                        row.get::<f32>(2).unwrap().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [2, 1]);
        assert!(rows[0].1 > rows[1].1);
    }

    #[pg_test]
    fn scoring_and_inspection_respect_partial_index_predicates() {
        Spi::run(
            "CREATE TABLE lite_partial_score (id int, body text, active boolean);
             INSERT INTO lite_partial_score VALUES
               (1, 'beer', true), (2, 'wine', true),
               (3, 'wine', NULL), (4, NULL, true);
             INSERT INTO lite_partial_score
               SELECT n, 'wine', false FROM generate_series(5, 104) AS n;
             CREATE INDEX lite_partial_score_idx ON lite_partial_score
               USING tin (body) WHERE active;
             CREATE TABLE lite_partial_score_control AS
               SELECT id, body FROM lite_partial_score WHERE active;
             CREATE INDEX lite_partial_score_control_idx ON lite_partial_score_control
               USING tin (body);",
        )
        .unwrap();
        let partial = Spi::get_one::<f32>(
            "SELECT tin.full_score(ctid) FROM lite_partial_score
             WHERE active AND body ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        let control = Spi::get_one::<f32>(
            "SELECT tin.full_score(ctid) FROM lite_partial_score_control
             WHERE body ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert!(control > 0.0);
        assert_eq!(partial, control);

        // In the indexed population, beer occurs in half the documents and
        // must be elided at the default dense ratio, despite the excluded rows.
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM tin.score_inspect('lite_partial_score_idx', 'beer')"
            )
            .unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<f32>(
                "SELECT tin.score(ctid) FROM lite_partial_score
                 WHERE active AND body ==> 'beer'"
            )
            .unwrap(),
            Some(0.0)
        );
    }

    #[pg_test]
    fn scoring_respects_partial_expression_index_predicates() {
        Spi::run(
            "CREATE TABLE lite_partial_expression (id int, body text, active boolean);
             INSERT INTO lite_partial_expression VALUES
               (1, 'BEER', true), (2, 'wine wine', true),
               (3, 'BEER BEER', false), (4, 'excluded', false),
               (5, 'excluded', NULL), (6, NULL, true);
             CREATE INDEX lite_partial_expression_idx ON lite_partial_expression
               USING tin (lower(body)) WHERE active OR id = 3;
             CREATE TABLE lite_partial_expression_control AS
               SELECT id, body FROM lite_partial_expression WHERE active OR id = 3;
             CREATE INDEX lite_partial_expression_control_idx
               ON lite_partial_expression_control USING tin (lower(body));",
        )
        .unwrap();
        let partial = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(tin.full_score(ctid) ORDER BY id)
             FROM lite_partial_expression
             WHERE (active OR id = 3) AND lower(body) ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        let control = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(tin.full_score(ctid) ORDER BY id)
             FROM lite_partial_expression_control WHERE lower(body) ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(control.len(), 2);
        assert_eq!(partial, control);
    }

    #[pg_test]
    fn highlighting_supports_explicit_and_implicit_queries() {
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT tin.highlight('Beer and wine', '[', ']', query => 'beer')"
            )
            .unwrap(),
            Some("[Beer] and wine".into())
        );
        Spi::run(
            "CREATE TABLE lite_highlight (id int, s1 text, s2 text);
             INSERT INTO lite_highlight VALUES
               (1, 'Beer', 'and wine'), (2, 'cider', 'only');
             CREATE INDEX lite_highlight_idx ON lite_highlight
               USING tin (((s1 || ' '::text) || s2));",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT tin.highlight(s1 || ' ' || s2)
                 FROM lite_highlight
                 WHERE (s1 || ' ' || s2) ==> 'beer'"
            )
            .unwrap(),
            Some("<b>Beer</b> and wine".into())
        );
        let ansi = Spi::get_one::<String>(
            "SELECT tin.highlight_ansi(s1 || ' ' || s2)
             FROM lite_highlight
             WHERE (s1 || ' ' || s2) ==> 'beer'",
        )
        .unwrap()
        .unwrap();
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("Beer"));
    }
}
