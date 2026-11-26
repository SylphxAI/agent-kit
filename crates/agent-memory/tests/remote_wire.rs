use agent_memory::remote::{
    Stamp, WireError, embedding_request, embedding_response, reported_cost, reported_input_tokens,
    rerank_request, rerank_response,
};
use serde_json::{Value, json};

#[test]
fn requests_preserve_input_bytes_and_only_add_opt_in_provider_policy() {
    let passages = ["界🙂\nfirst\0tail".to_owned(), "e\u{301}".to_owned()];
    let query = "query\n界\0tail";
    assert_eq!(
        embedding_request("embed", &passages, None, false),
        json!({"model": "embed", "input": passages, "encoding_format": "float"})
    );
    assert_eq!(
        embedding_request("embed", &passages, Some(384), true),
        json!({
            "model": "embed", "input": passages, "encoding_format": "float", "dimensions": 384,
            "provider": {"data_collection": "deny"},
        })
    );
    assert_eq!(
        rerank_request("rerank", query, &passages, false),
        json!({"model": "rerank", "query": query, "documents": passages, "top_n": 2})
    );
    assert_eq!(
        rerank_request("rerank", query, &passages, true),
        json!({
            "model": "rerank", "query": query, "documents": passages, "top_n": 2,
            "provider": {"data_collection": "deny"},
        })
    );
}

#[test]
fn responses_restore_input_order_without_normalizing_or_clamping() {
    let embeddings = json!({"data": [
        {"index": 1, "embedding": [f32::MIN, f32::MAX]},
        {"index": 0, "embedding": [3, 4]},
    ]});
    assert_eq!(
        embedding_response(&embeddings, 2).unwrap(),
        [vec![3.0, 4.0], vec![f32::MIN, f32::MAX]]
    );
    for (array, score) in [("results", "relevance_score"), ("data", "score")] {
        let response = json!({(array): [
            {"index": 1, (score): 0.5}, {"index": 0, (score): -2},
        ]});
        assert_eq!(rerank_response(&response, 2).unwrap(), [-2.0, 0.5]);
    }
    assert!(
        embedding_response(&json!({"data": []}), 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        rerank_response(&json!({"results": []}), 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn both_decoders_reject_missing_duplicate_invalid_or_incomplete_indices() {
    for (rows, error) in [
        (
            vec![json!({"index": 0})],
            WireError::ResultCount {
                expected: 2,
                actual: 1,
            },
        ),
        (
            vec![json!({"index": 0}); 3],
            WireError::ResultCount {
                expected: 2,
                actual: 3,
            },
        ),
        (
            vec![json!({"index": 0}); 2],
            WireError::DuplicateIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({})],
            WireError::InvalidIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({"index": 2})],
            WireError::InvalidIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({"index": u64::MAX})],
            WireError::InvalidIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({"index": -1})],
            WireError::InvalidIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({"index": 1.0})],
            WireError::InvalidIndex { row: 1 },
        ),
        (
            vec![json!({"index": 0}), json!({"index": "1"})],
            WireError::InvalidIndex { row: 1 },
        ),
    ] {
        let mut rows = rows;
        for row in &mut rows {
            row["embedding"] = json!([1.0]);
            row["relevance_score"] = json!(0.5);
        }
        assert_eq!(embedding_response(&json!({"data": rows}), 2), Err(error));
        assert_eq!(rerank_response(&json!({"results": rows}), 2), Err(error));
    }
    // An untrusted envelope cannot make the requested count drive an allocation.
    assert!(matches!(
        embedding_response(&json!({"data": []}), usize::MAX),
        Err(WireError::ResultCount { actual: 0, .. })
    ));
}

#[test]
fn malformed_numeric_values_are_not_dropped_or_echoed() {
    let private = "private provider diagnostic sentinel";
    for invalid in [
        json!(private),
        json!(null),
        json!({}),
        json!(true),
        json!(1e100),
        json!(-1e100),
    ] {
        let err = embedding_response(
            &json!({"data": [{"index": 0, "embedding": [1, invalid]}]}),
            1,
        )
        .unwrap_err();
        assert_eq!(err, WireError::InvalidEmbedding { row: 0 });
        assert!(!err.to_string().contains(private));
        assert!(!format!("{err:?}").contains(private));
        let err = rerank_response(
            &json!({"results": [{"index": 0, "relevance_score": invalid}]}),
            1,
        )
        .unwrap_err();
        assert_eq!(err, WireError::InvalidScore { row: 0 });
        assert!(!err.to_string().contains(private));
        assert!(!format!("{err:?}").contains(private));
    }
    assert_eq!(
        embedding_response(&json!({"data": [{"index": 0, "embedding": []}]}), 1),
        Err(WireError::InvalidEmbedding { row: 0 })
    );
    assert_eq!(
        embedding_response(
            &json!({"data": [
                {"index": 0, "embedding": [1, 2]}, {"index": 1, "embedding": [3]},
            ]}),
            2
        ),
        Err(WireError::InconsistentDimensions { row: 1 })
    );
    assert_eq!(
        rerank_response(&json!({"error": private}), 1),
        Err(WireError::MissingResults)
    );
    assert_eq!(
        embedding_response(&json!({"data": private}), 1),
        Err(WireError::MissingResults)
    );
}

#[test]
fn usage_extracts_reported_values_without_estimates() {
    let embedding = json!({"usage": {"prompt_tokens": 29, "total_tokens": 37, "cost": 0.001}});
    assert_eq!(reported_input_tokens(&embedding), Some(29));
    assert_eq!(reported_cost(&embedding), Some(0.001));

    let cohere = json!({"meta": {"billed_units": {"input_tokens": 51, "search_units": 1}}});
    assert_eq!(reported_input_tokens(&cohere), Some(51));
    assert_eq!(reported_cost(&cohere), None);

    let jina = json!({"usage": {"total_tokens": 61}});
    assert_eq!(reported_input_tokens(&jina), Some(61));
    assert_eq!(reported_cost(&jina), None);

    let zero = json!({"usage": {"prompt_tokens": "bad", "input_tokens": 0, "cost": 0}});
    assert_eq!(reported_input_tokens(&zero), Some(0));
    assert_eq!(reported_cost(&zero), Some(0.0));

    for bad in [Value::Null, json!(-1), json!("1"), json!(1.5)] {
        assert_eq!(
            reported_input_tokens(&json!({"usage": {"total_tokens": bad}})),
            None
        );
    }
    for bad in [Value::Null, json!(-1), json!("1")] {
        assert_eq!(reported_cost(&json!({"usage": {"cost": bad}})), None);
    }
    assert_eq!(
        reported_input_tokens(&json!({"usage": {"search_units": 1}})),
        None
    );
    assert_eq!(reported_cost(&json!({"data": []})), None);
}

#[test]
fn stamp_keeps_its_persisted_identity_with_or_without_service() {
    let original =
        r#"{"provider":"remote","model":"embed-v1","dimensions":384,"preprocessing":"project-v1"}"#;
    let stamp: Stamp = serde_json::from_str(original).unwrap();
    assert_eq!(stamp.key(), original);
    assert_eq!(stamp.digest(), "5e30b62ea4460a12");
    #[cfg(feature = "service")]
    {
        let legacy: agent_memory::providers::Stamp = stamp.clone();
        assert_eq!(legacy, stamp);
    }
}
