package source

import "testing"

func TestFilterAfterSuppressesRewindDuplicates(t *testing.T) {
	var versions []int64
	emit := filterAfter(100, func(version int64, _ string) error {
		versions = append(versions, version)
		return nil
	})
	for _, version := range []int64{98, 99, 100, 101, 102} {
		if err := emit(version, "entry"); err != nil {
			t.Fatal(err)
		}
	}
	if len(versions) != 2 || versions[0] != 101 || versions[1] != 102 {
		t.Fatalf("unexpected emitted versions: %v", versions)
	}
}
