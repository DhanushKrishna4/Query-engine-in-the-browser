//! Round trips, and predicates answered without decoding.
//!
//! Every encoding is checked two ways: decoding it must reproduce the column
//! exactly, and evaluating a predicate on the encoded form must agree with
//! evaluating it on the decoded one. The second is the interesting property --
//! an encoding that compresses well and answers a predicate differently is
//! worse than no encoding at all.

use super::*;
use crate::storage::column::ColumnBuilder;

fn utf8(values: &[Option<&str>]) -> Column {
    let mut b = ColumnBuilder::new(&DataType::Utf8);
    for v in values {
        match v {
            Some(s) => b.append(&ScalarValue::Utf8((*s).into())).unwrap(),
            None => b.append_null(),
        }
    }
    b.finish()
}

fn i64s(values: &[Option<i64>]) -> Column {
    let mut b = ColumnBuilder::new(&DataType::Int64);
    for v in values {
        match v {
            Some(x) => b.append(&ScalarValue::Int64(*x)).unwrap(),
            None => b.append_null(),
        }
    }
    b.finish()
}

fn i32s(values: &[i32]) -> Column {
    let mut b = ColumnBuilder::new(&DataType::Int32);
    for v in values {
        b.append(&ScalarValue::Int32(*v)).unwrap();
    }
    b.finish()
}

/// Decode and compare value by value, NULLs included.
fn assert_round_trip(original: &Column, encoded: &Encoded) {
    let decoded = decode(encoded, original.validity.clone());
    assert_eq!(decoded.len(), original.len(), "{} length", encoded.name());
    for i in 0..original.len() {
        assert_eq!(
            decoded.is_valid(i),
            original.is_valid(i),
            "{} validity at {i}",
            encoded.name()
        );
        if original.is_valid(i) {
            assert_eq!(
                decoded.value(i),
                original.value(i),
                "{} value at {i}",
                encoded.name()
            );
        }
    }
}

/// The encoded predicate must agree with the plain one, row for row.
fn assert_predicate_agrees(column: &Column, encoded: &Encoded, op: BinaryOperator, v: &ScalarValue) {
    let Some(fast) = evaluate(encoded, op, v, column.validity.as_ref()) else {
        return; // this encoding declines the predicate, which is allowed
    };
    for i in 0..column.len() {
        let want = column.is_valid(i)
            && types::compare(&column.value(i), v).is_some_and(|o| compare_op(o, op));
        assert_eq!(
            fast.get(i),
            want,
            "{} disagreed at row {i} for {op:?} {v}",
            encoded.name()
        );
    }
}

// ---------------------------------------------------------------------------
// Dictionary
// ---------------------------------------------------------------------------

#[test]
fn a_low_cardinality_text_column_is_dictionary_encoded() {
    let cities: Vec<Option<&str>> = (0..500)
        .map(|i| match i % 5 {
            0 => Some("London"),
            1 => Some("New York"),
            2 => Some("Austin"),
            3 => None,
            _ => Some("Cambridge"),
        })
        .collect();
    let column = utf8(&cities);
    let encoded = encode(&column).expect("should encode");
    assert_eq!(encoded.name(), "dictionary");
    assert_round_trip(&column, &encoded);

    // Four distinct values, so the dictionary is four entries however long the
    // column is.
    let Encoded::Dictionary { values, .. } = &encoded else {
        panic!("expected a dictionary");
    };
    assert_eq!(values.len(), 4);
    assert!(encoded.byte_size() * 2 < column.data.byte_size());
}

#[test]
fn dictionary_codes_preserve_order() {
    // The dictionary is sorted, which is what lets `<` and `>` be answered by
    // comparing codes rather than strings.
    let names = ["delta", "alpha", "charlie", "bravo"];
    let cells: Vec<Option<&str>> = (0..40).map(|i| Some(names[i % 4])).collect();
    let column = utf8(&cells);
    let encoded = dictionary(&column).unwrap();
    let Encoded::Dictionary { values, codes } = &encoded else {
        panic!()
    };
    let sorted: Vec<&str> = (0..values.len()).map(|i| values.get(i)).collect();
    assert_eq!(sorted, vec!["alpha", "bravo", "charlie", "delta"]);
    // "delta" is the largest, so it has the largest code.
    assert_eq!(codes[0], 3, "delta is the largest, so the largest code");
    assert_eq!(codes[1], 0, "alpha is the smallest");
}

#[test]
fn a_dictionary_answers_every_comparison() {
    let column = utf8(&[
        Some("London"),
        Some("Austin"),
        None,
        Some("Cambridge"),
        Some("London"),
        Some("Austin"),
    ]);
    let encoded = dictionary(&column).unwrap();
    for op in [
        BinaryOperator::Eq,
        BinaryOperator::NotEq,
        BinaryOperator::Lt,
        BinaryOperator::LtEq,
        BinaryOperator::Gt,
        BinaryOperator::GtEq,
    ] {
        for needle in ["Austin", "Cambridge", "London", "Zanzibar", "AAA"] {
            assert_predicate_agrees(
                &column,
                &encoded,
                op,
                &ScalarValue::Utf8(needle.into()),
            );
        }
    }
}

#[test]
fn a_high_cardinality_text_column_is_not_dictionary_encoded() {
    let values: Vec<String> = (0..200).map(|i| format!("token-{i}")).collect();
    let refs: Vec<Option<&str>> = values.iter().map(|s| Some(s.as_str())).collect();
    let column = utf8(&refs);
    assert!(dictionary(&column).is_none(), "every value is distinct");
}

// ---------------------------------------------------------------------------
// Run-length
// ---------------------------------------------------------------------------

#[test]
fn a_column_of_long_runs_is_run_length_encoded() {
    // Ten runs of a hundred.
    let values: Vec<Option<i64>> = (0..1000).map(|i| Some(i / 100)).collect();
    let column = i64s(&values);
    let encoded = run_length(&column).expect("should encode");
    let Encoded::RunLength { ends, .. } = &encoded else {
        panic!()
    };
    assert_eq!(ends.len(), 10);
    assert_round_trip(&column, &encoded);
    assert!(encoded.byte_size() * 10 < column.data.byte_size());
}

#[test]
fn run_length_handles_runs_of_nulls() {
    // Long enough that four runs are worth encoding; the shape is
    // 1s | NULLs | 2s | NULLs.
    let mut values: Vec<Option<i64>> = Vec::new();
    values.extend(std::iter::repeat_n(Some(1), 20));
    values.extend(std::iter::repeat_n(None, 30));
    values.extend(std::iter::repeat_n(Some(2), 20));
    values.extend(std::iter::repeat_n(None, 10));
    let column = i64s(&values);
    let encoded = run_length(&column).unwrap();
    let Encoded::RunLength { ends, .. } = &encoded else {
        panic!()
    };
    assert_eq!(ends.as_slice(), &[20, 50, 70, 80]);
    assert_round_trip(&column, &encoded);

    // And a NULL run must not answer a predicate: comparing against NULL is
    // UNKNOWN, never TRUE, however the run is stored.
    let hits = evaluate(
        &encoded,
        BinaryOperator::Eq,
        &ScalarValue::Int64(1),
        column.validity.as_ref(),
    )
    .unwrap();
    assert_eq!(hits.count_set(), 20);
    assert!(hits.get(0) && !hits.get(20));
}

#[test]
fn run_length_answers_comparisons_one_run_at_a_time() {
    let values: Vec<Option<i64>> = (0..600)
        .map(|i| if i % 200 == 199 { None } else { Some(i / 200) })
        .collect();
    let column = i64s(&values);
    let encoded = run_length(&column).unwrap();
    for op in [
        BinaryOperator::Eq,
        BinaryOperator::Lt,
        BinaryOperator::GtEq,
        BinaryOperator::NotEq,
    ] {
        for target in [-1i64, 0, 1, 2, 9] {
            assert_predicate_agrees(&column, &encoded, op, &ScalarValue::Int64(target));
        }
    }
}

#[test]
fn a_column_with_no_runs_is_not_run_length_encoded() {
    let values: Vec<Option<i64>> = (0..500).map(|i| Some(i * 7 % 499)).collect();
    assert!(run_length(&i64s(&values)).is_none());
}

// ---------------------------------------------------------------------------
// Bit packing and frame of reference
// ---------------------------------------------------------------------------

#[test]
fn a_small_integer_range_is_bit_packed() {
    // Values 0..6 need three bits, not thirty-two.
    let values: Vec<i32> = (0..2000).map(|i| i % 7).collect();
    let column = i32s(&values);
    let encoded = bit_packed(&column).expect("should encode");
    let Encoded::BitPacked { bit_width, reference, .. } = &encoded else {
        panic!()
    };
    assert_eq!(*bit_width, 3);
    assert_eq!(*reference, 0);
    assert_round_trip(&column, &encoded);
    // Three bits against thirty-two.
    assert!(encoded.byte_size() * 8 < column.data.byte_size());
}

#[test]
fn a_constant_column_needs_no_bits_at_all() {
    let column = i32s(&vec![42; 1000]);
    let encoded = bit_packed(&column).unwrap();
    let Encoded::BitPacked { bit_width, reference, .. } = &encoded else {
        panic!()
    };
    assert_eq!(*bit_width, 0, "one distinct value is zero bits of range");
    assert_eq!(*reference, 42);
    assert_round_trip(&column, &encoded);
}

#[test]
fn negative_values_pack_against_their_minimum() {
    let values: Vec<i32> = (-500..500).collect();
    let column = i32s(&values);
    let encoded = bit_packed(&column).unwrap();
    let Encoded::BitPacked { reference, bit_width, .. } = &encoded else {
        panic!()
    };
    assert_eq!(*reference, -500);
    assert_eq!(*bit_width, 10);
    assert_round_trip(&column, &encoded);
}

#[test]
fn a_climbing_column_packs_better_per_block() {
    // Ascending over a million: one global minimum leaves a 20-bit span, but
    // each block of 1024 spans only 1024. This is what frame-of-reference is
    // for, and it should beat plain bit-packing here.
    let values: Vec<Option<i64>> = (0..8000).map(|i| Some(1_000_000 + i)).collect();
    let column = i64s(&values);
    let packed = bit_packed(&column).unwrap();
    let forr = frame_of_reference(&column).unwrap();
    assert!(
        forr.byte_size() < packed.byte_size(),
        "frame-of-reference {} vs bit-packed {}",
        forr.byte_size(),
        packed.byte_size()
    );
    assert_round_trip(&column, &forr);
    // Eleven bits per block instead of thirteen for the whole column.
    let Encoded::FrameOfReference { blocks, .. } = &forr else {
        panic!()
    };
    assert_eq!(blocks.len(), 8000_usize.div_ceil(BLOCK));
    assert!(blocks.iter().all(|b| b.bit_width <= 10));
}

#[test]
fn bit_packed_and_frame_of_reference_answer_comparisons() {
    for column in [
        i32s(&(0..3000).map(|i| i % 50).collect::<Vec<_>>()),
        i32s(&(0..3000).collect::<Vec<_>>()),
        i32s(&vec![7; 3000]),
    ] {
        for encoded in [bit_packed(&column), frame_of_reference(&column)]
            .into_iter()
            .flatten()
        {
            for op in [
                BinaryOperator::Eq,
                BinaryOperator::Lt,
                BinaryOperator::LtEq,
                BinaryOperator::Gt,
                BinaryOperator::GtEq,
                BinaryOperator::NotEq,
            ] {
                for target in [-5i32, 0, 7, 25, 49, 1500, 5000] {
                    assert_predicate_agrees(
                        &column,
                        &encoded,
                        op,
                        &ScalarValue::Int32(target),
                    );
                }
            }
        }
    }
}

#[test]
fn a_block_outside_the_predicate_is_never_unpacked() {
    // Ascending, so each block's range is disjoint from the others'. A
    // predicate matching only the first block must leave the rest untouched --
    // which is not directly observable, so this checks the answer instead and
    // the block ranges that drive it.
    let values: Vec<Option<i64>> = (0..8000).map(Some).collect();
    let column = i64s(&values);
    let encoded = frame_of_reference(&column).unwrap();
    let hits = evaluate(&encoded, BinaryOperator::Lt, &ScalarValue::Int64(500), None).unwrap();
    assert_eq!(hits.count_set(), 500);
    assert!(hits.get(499) && !hits.get(500));

    let Encoded::FrameOfReference { blocks, .. } = &encoded else {
        panic!()
    };
    // Only the first block can contain anything below 500.
    assert!(blocks[1].reference >= 500);
}

// ---------------------------------------------------------------------------
// Choosing between them
// ---------------------------------------------------------------------------

#[test]
fn the_smallest_encoding_that_pays_is_chosen() {
    // Long runs of a small integer: both run-length and bit-packing apply, and
    // run-length is far smaller.
    let values: Vec<Option<i64>> = (0..4000).map(|i| Some(i / 500)).collect();
    let column = i64s(&values);
    assert_eq!(encode(&column).unwrap().name(), "run-length");

    // No runs, small range: bit-packing.
    let values: Vec<Option<i64>> = (0..4000).map(|i| Some(i * 7 % 13)).collect();
    assert_eq!(encode(&i64s(&values)).unwrap().name(), "bit-packed");

    // Ascending over a wide range: frame-of-reference.
    let values: Vec<Option<i64>> = (0..8000).map(|i| Some(500_000_000 + i * 3)).collect();
    assert_eq!(encode(&i64s(&values)).unwrap().name(), "frame-of-reference");
}

#[test]
fn an_incompressible_column_is_left_alone() {
    // Distinct strings with nothing repeating.
    let values: Vec<String> = (0..300).map(|i| format!("{i}-{}", i * 7919)).collect();
    let refs: Vec<Option<&str>> = values.iter().map(|s| Some(s.as_str())).collect();
    assert!(encode(&utf8(&refs)).is_none());

    // Float columns have no integer view and no dictionary path.
    let mut b = ColumnBuilder::new(&DataType::Float64);
    for i in 0..1000 {
        b.append(&ScalarValue::Float64(i as f64 * 1.7)).unwrap();
    }
    assert!(encode(&b.finish()).is_none());
}

#[test]
fn an_empty_column_encodes_to_nothing() {
    let column = i64s(&[]);
    assert!(encode(&column).is_none());
}

// ---------------------------------------------------------------------------
// Bit packing itself
// ---------------------------------------------------------------------------

#[test]
fn bits_survive_straddling_word_boundaries() {
    // Widths that do not divide 64, so values cross words constantly.
    for width in [1u32, 3, 7, 13, 17, 31, 33, 63] {
        let count = 200;
        let values: Vec<u64> = (0..count as u64).map(|i| i & mask(width)).collect();
        let mut w = BitWriter::new(count * width as usize);
        for v in &values {
            w.put(*v, width);
        }
        let words = w.finish();
        let r = BitReader::new(&words);
        for (i, want) in values.iter().enumerate() {
            assert_eq!(
                r.get(i * width as usize, width),
                *want,
                "width {width}, value {i}"
            );
        }
    }
}

#[test]
fn a_zero_width_field_reads_back_as_zero() {
    let mut w = BitWriter::new(0);
    for _ in 0..100 {
        w.put(0, 0);
    }
    let words = w.finish();
    let r = BitReader::new(&words);
    assert_eq!(r.get(0, 0), 0);
    assert_eq!(r.get(500, 0), 0);
}
