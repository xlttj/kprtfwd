package discovery

import "testing"

func TestMatchesWildcardPattern(t *testing.T) {
	cases := []struct {
		text    string
		pattern string
		want    bool
	}{
		{"anything", "*", true},
		{"", "*", true},
		{"", "", true},
		{"text", "", false},
		{"my-app-prod", "my-app-*", true},
		{"other-prod", "my-app-*", false},
		{"team-staging", "*-staging", true},
		{"team-prod", "*-staging", false},
		{"a-middle-b", "*middle*", true},
		{"a-end-b", "*middle*", false},
		{"exact", "exact", true},
		{"exact", "exact2", false},
		{"prefix-only", "prefix", false},
	}

	for _, tc := range cases {
		if got := MatchesWildcardPattern(tc.text, tc.pattern); got != tc.want {
			t.Errorf("MatchesWildcardPattern(%q, %q) = %t, want %t", tc.text, tc.pattern, got, tc.want)
		}
	}
}
