use kasu_derived::*;
use proptest::prelude::*;
proptest! {
    #[test]
    fn line_patch_reconstructs_bytes_and_matches_are_monotone(a in prop::collection::vec(any::<u8>(),0..1024),b in prop::collection::vec(any::<u8>(),0..1024)) {
        let old=lines(&a);let new=lines(&b);let mut output=Vec::new();let mut offset=0;
        for edit in line_edits(&a,&b) {
            for line in &old[offset..edit.before.start] {output.extend_from_slice(line);}
            for line in &new[edit.after.clone()] {output.extend_from_slice(line);}
            offset=edit.before.end;
        }
        for line in &old[offset..] {output.extend_from_slice(line);}
        prop_assert_eq!(&output,&b);
        let matching=matching_lines(&a,&b);
        for &(a,b) in &matching {prop_assert_eq!(old[a],new[b]);}
        for pair in matching.windows(2) {prop_assert!(pair[0].0<pair[1].0 && pair[0].1<pair[1].1);}
    }
}
