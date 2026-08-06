use super::*;

fn term(ordinal: usize, kind: TermKind, bytes: &[u8]) -> Term {
    Term::new(ordinal, kind, "label", bytes.to_vec(), false).unwrap()
}

fn region(start: usize, end: usize, terms: Vec<Term>) -> ConflictRegion {
    ConflictRegion::new(
        ByteRange::new(start, end).unwrap(),
        SnapshotMarker::default(),
        terms,
    )
    .unwrap()
}

fn manifest_term(region_index: usize, ordinal: usize) -> ManifestTerm {
    let value = term(ordinal, TermKind::Side, b"x\n");
    ManifestTerm::from_term(region_index, &value, Sha256Digest::ZERO)
}

#[test]
fn byte_ranges_are_checked_and_half_open() {
    assert_eq!(ByteRange::new(2, 2).unwrap().len(), 0);
    assert_eq!(ByteRange::new(2, 5).unwrap().len(), 3);
    assert!(ByteRange::new(5, 2).is_err());
    assert!(ByteRange::from_start_len(usize::MAX, 1).is_err());
    let malformed = ByteRange { start: 5, end: 2 };
    assert!(malformed.validate().is_err());
    assert_eq!(malformed.len(), 0);
    let range = ByteRange::new(2, 5).unwrap();
    assert!(range.contains_offset(2));
    assert!(!range.contains_offset(5));
    assert_eq!(range.slice(b"012345").unwrap(), b"234");
}

#[test]
fn terms_allow_empty_bytes_arbitrary_arity_and_repeated_kinds() {
    let terms = vec![
        term(0, TermKind::Side, b""),
        term(1, TermKind::Base, b"base"),
        term(2, TermKind::Base, b""),
        term(3, TermKind::Side, b"side"),
        term(4, TermKind::Base, b"base-2"),
    ];
    assert_eq!(Term::assemble(terms.clone()).unwrap(), terms);
    assert_eq!(terms[0].logical_len(), 0);
    assert!(!terms[0].logical_final_newline());
    assert!(Term::assemble(vec![term(1, TermKind::Side, b"x")]).is_err());
    assert!(
        Term::assemble(vec![
            term(0, TermKind::Side, b"x"),
            term(2, TermKind::Base, b"y")
        ])
        .is_err()
    );
}

#[test]
fn regions_and_documents_allow_gaps_and_touching_ranges_but_not_overlap() {
    let marker = SnapshotMarker::default();
    let first = region(0, 3, vec![term(0, TermKind::Side, b"a")]);
    let second = region(3, 6, vec![term(0, TermKind::Side, b"b")]);
    assert!(ParsedDocument::new(b"abcdef".to_vec(), vec![first, second], marker).is_ok());

    let first = region(0, 3, vec![term(0, TermKind::Side, b"a")]);
    let second = region(2, 6, vec![term(0, TermKind::Side, b"b")]);
    assert!(ParsedDocument::new(b"abcdef".to_vec(), vec![first, second], marker).is_err());

    let first = region(4, 6, vec![term(0, TermKind::Side, b"a")]);
    let second = region(1, 3, vec![term(0, TermKind::Side, b"b")]);
    assert!(ParsedDocument::new(b"abcdef".to_vec(), vec![first, second], marker).is_err());
}

#[test]
fn manifest_validates_schema_ranges_indices_and_exact_paths() {
    let source = SourceIdentity::new("source");
    let marker = SnapshotMarker::default();
    let valid_region = ManifestRegion {
        region_index: 0,
        source_range: ByteRange::new(1, 2).unwrap(),
        seed: b"seed".to_vec().into_boxed_slice(),
        terms: vec![manifest_term(0, 0)],
    };
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![valid_region.clone()]
        )
        .is_ok()
    );
    assert!(
        Manifest::new(
            1,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![valid_region.clone()]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.region_index = 1;
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.source_range = ByteRange::new(2, 4).unwrap();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.terms[0].artifact_path = "../escape.term".into();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.terms[0].artifact_path = "/absolute/escape.term".into();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.terms[0].artifact_path = "regions/region-000/term-001.term".into();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.terms.clear();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.seed = Vec::<u8>::new().into_boxed_slice();
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.region_index = 1;
    let second = ManifestRegion {
        region_index: 1,
        source_range: ByteRange::new(1, 2).unwrap(),
        seed: b"seed".to_vec().into_boxed_slice(),
        terms: vec![manifest_term(1, 0)],
    };
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid, second]
        )
        .is_err()
    );

    let mut invalid = valid_region.clone();
    invalid.source_range = ByteRange::new(0, 2).unwrap();
    let second = ManifestRegion {
        region_index: 1,
        source_range: ByteRange::new(1, 3).unwrap(),
        seed: b"seed".to_vec().into_boxed_slice(),
        terms: vec![manifest_term(1, 0)],
    };
    assert!(
        Manifest::new(
            MANIFEST_SCHEMA_VERSION,
            source.clone(),
            Sha256Digest::ZERO,
            marker,
            3,
            vec![invalid, second]
        )
        .is_err()
    );

    let invalid_marker = SnapshotMarker {
        outer_marker_width: 6,
        ..marker
    };
    assert!(
        Manifest::new(
            1,
            source.clone(),
            Sha256Digest::ZERO,
            invalid_marker,
            3,
            vec![]
        )
        .is_err()
    );

    let derived = ManifestTerm::from_term(0, &term(0, TermKind::Side, b"x\n"), Sha256Digest::ZERO);
    assert_eq!(derived.logical_length, 2);
    assert!(derived.logical_final_newline);
    assert_eq!(
        derived.artifact_path,
        ManifestTerm::generated_artifact_path(0, 0)
    );

    assert_eq!(
        ManifestTerm::generated_artifact_path(1000, 1001),
        std::path::PathBuf::from("regions/region-1000/term-1001.term")
    );
}

#[test]
fn apply_plan_supports_insert_delete_replace_adjacent_and_noop() {
    let insertion = DiffHunk::new(0, ByteRange::new(1, 1).unwrap(), b"insert".to_vec());
    let deletion = DiffHunk::new(1, ByteRange::new(1, 3).unwrap(), Vec::<u8>::new());
    let replacement = DiffHunk::new(2, ByteRange::new(3, 4).unwrap(), b"new".to_vec());
    assert!(
        ApplyPlan::new(vec![insertion, deletion, replacement])
            .unwrap()
            .validate(5)
            .is_ok()
    );
    assert!(ApplyPlan::empty().validate(0).is_ok());

    let mut inconsistent = DiffHunk::new(0, ByteRange::new(0, 1).unwrap(), b"x".to_vec());
    inconsistent.old_len = 99;
    assert!(ApplyPlan::new(vec![inconsistent]).is_err());
    let mut inconsistent = DiffHunk::new(0, ByteRange::new(0, 1).unwrap(), b"x".to_vec());
    inconsistent.new_len = 99;
    assert!(ApplyPlan::new(vec![inconsistent]).is_err());
    let malformed = DiffHunk {
        sequence: 0,
        original_range: ByteRange { start: 3, end: 1 },
        replacement: Box::new([]),
        old_len: 0,
        new_len: 0,
    };
    assert!(ApplyPlan::new(vec![malformed]).is_err());

    let overlap = vec![
        DiffHunk::new(0, ByteRange::new(0, 2).unwrap(), b"x".to_vec()),
        DiffHunk::new(1, ByteRange::new(1, 3).unwrap(), b"y".to_vec()),
    ];
    assert!(ApplyPlan::new(overlap).is_err());
    let reversed = vec![
        DiffHunk::new(0, ByteRange::new(2, 3).unwrap(), b"x".to_vec()),
        DiffHunk::new(1, ByteRange::new(0, 1).unwrap(), b"y".to_vec()),
    ];
    assert!(ApplyPlan::new(reversed).is_err());
    let out_of_source = ApplyPlan::new(vec![DiffHunk::new(
        0,
        ByteRange::new(4, 6).unwrap(),
        b"x".to_vec(),
    )])
    .unwrap();
    assert!(out_of_source.validate(5).is_err());
    let boundary_insertion = ApplyPlan::new(vec![DiffHunk::new(
        0,
        ByteRange::new(5, 5).unwrap(),
        b"x".to_vec(),
    )])
    .unwrap();
    assert!(boundary_insertion.validate(5).is_ok());
    let non_contiguous = vec![DiffHunk {
        sequence: 1,
        ..DiffHunk::new(0, ByteRange::new(0, 0).unwrap(), b"x".to_vec())
    }];
    assert!(ApplyPlan::new(non_contiguous).is_err());
}
