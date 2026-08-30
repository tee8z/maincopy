#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

readonly threshold=20

usage() {
  cat <<'EOF'
Usage: scripts/crap-report.sh [--output-dir DIR] [--input-dir DIR]

Measure the workspace and write a CRAP report below target/crap by default.
Use --input-dir to rebuild a report from saved coverage.lcov and complexity.json.
Set KNOTS_BIN to select the Knots 1.16.0 executable.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir
root=$(cd -- "$script_dir/.." && pwd -P)
readonly root
output_dir="$root/target/crap"
input_dir=

while (($#)); do
  case $1 in
    --output-dir | --input-dir)
      (($# >= 2)) || die "$1 requires a value"
      if [[ $1 == --output-dir ]]; then output_dir=$2; else input_dir=$2; fi
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

for command in awk basename cat cp dirname find mkdir mktemp mv sort; do
  command -v "$command" >/dev/null || die "$command is required"
done

# Resolve an existing workspace file without GNU-only realpath options.
workspace_path() {
  local candidate directory base physical
  candidate=$1
  [[ $candidate == /* ]] || candidate="$root/$candidate"
  directory=$(dirname "$candidate")
  base=$(basename "$candidate")
  physical=$(cd -- "$directory" 2>/dev/null && pwd -P) || return 1
  candidate="$physical/$base"
  [[ -f $candidate && $candidate == "$root/"* ]] || return 1
  printf '%s\n' "${candidate#"$root/"}"
}

# Resolve a future output directory and reject traversal outside target.
output_path() {
  local candidate suffix= physical
  candidate=$1
  [[ $candidate == /* ]] || candidate="$root/$candidate"
  case "/$candidate/" in */../*) return 1 ;; esac
  while [[ ! -d $candidate ]]; do
    [[ $candidate != / ]] || return 1
    suffix="/$(basename "$candidate")$suffix"
    candidate=$(dirname "$candidate")
  done
  physical=$(cd -- "$candidate" && pwd -P) || return 1
  candidate="$physical$suffix"
  [[ $candidate == "$root/target/"* ]] || return 1
  printf '%s\n' "$candidate"
}

output_dir=$(output_path "$output_dir") || die "--output-dir must be below target"
readonly output_dir
mkdir -p "$root/target"
work_dir=$(mktemp -d "$root/target/.crap-report.XXXXXX")
readonly work_dir
cleanup() {
  find "$work_dir" -type f -delete
  find "$work_dir" -depth -type d -empty -delete
}
trap cleanup EXIT

if [[ -n $input_dir ]]; then
  [[ $input_dir == /* ]] || input_dir="$root/$input_dir"
  input_dir=$(cd -- "$input_dir" 2>/dev/null && pwd -P) || die "invalid --input-dir"
  [[ -f $input_dir/coverage.lcov && -f $input_dir/complexity.json ]] ||
    die "--input-dir requires coverage.lcov and complexity.json"
  cp "$input_dir/coverage.lcov" "$work_dir/coverage.lcov"
  cp "$input_dir/complexity.json" "$work_dir/complexity.json"
else
  for command in cargo; do command -v "$command" >/dev/null || die "$command is required"; done
  knots_bin=$(command -v "${KNOTS_BIN:-knots}" 2>/dev/null) ||
    die "knots 1.16.0 is required; set KNOTS_BIN to its path"
  cargo_version=$(cargo llvm-cov --version 2>/dev/null || true)
  [[ $cargo_version == "cargo-llvm-cov 0.8.5" ]] ||
    die "required cargo-llvm-cov 0.8.5; found ${cargo_version:-none}"
  knots_version=$("$knots_bin" --version 2>/dev/null || true)
  [[ $knots_version == "knots 1.16.0" ]] ||
    die "required knots 1.16.0; found ${knots_version:-none}"

  cd -- "$root"
  cargo llvm-cov --all-targets --all-features --include-build-script \
    --locked --offline --lcov --output-path "$work_dir/coverage.lcov"
  "$knots_bin" --recursive --language rust --count-anonymous-closures \
    --format json src tests build.rs build_support >"$work_dir/complexity.json"
fi

# Canonicalize LCOV source records and merge duplicate compilation contexts.
raw_records=0
current=
: >"$work_dir/coverage.raw.tsv"
while IFS= read -r record || [[ -n $record ]]; do
  case $record in
    SF:*)
      if current=$(workspace_path "${record#SF:}"); then
        raw_records=$((raw_records + 1))
      else
        current=
      fi
      ;;
    DA:*)
      [[ -n $current ]] || continue
      payload=${record#DA:}
      [[ $payload == *,* ]] || die "invalid LCOV record: $record"
      line=${payload%%,*}
      payload=${payload#*,}
      hits=${payload%%,*}
      [[ $line =~ ^[0-9]+$ && $line -gt 0 && $hits =~ ^[0-9]+$ ]] ||
        die "invalid LCOV record: $record"
      printf '%s\t%s\t%s\n' "$current" "$line" "$hits" >>"$work_dir/coverage.raw.tsv"
      ;;
    end_of_record) current= ;;
  esac
done <"$work_dir/coverage.lcov"

sort -t $'\t' -k1,1 -k2,2n "$work_dir/coverage.raw.tsv" |
  awk -F '\t' -v OFS='\t' '
    function emit() { if (file != "") print file, line, hits }
    $1 != file || $2 != line { emit(); file=$1; line=$2; hits=$3; next }
    $3 > hits { hits=$3 }
    END { emit() }
  ' >"$work_dir/coverage.tsv"

# Knots 1.16.0 emits a flat JSON object for each named function.
awk '
  function text_value(value) {
    sub(/^[^:]*:[[:space:]]*"/, "", value)
    sub(/",?[[:space:]]*$/, "", value)
    return value
  }
  function number_value(value) {
    sub(/^[^:]*:[[:space:]]*/, "", value)
    sub(/,?[[:space:]]*$/, "", value)
    return value
  }
  /"file"[[:space:]]*:/ { file=text_value($0) }
  /"function"[[:space:]]*:/ { name=text_value($0) }
  /"start_line"[[:space:]]*:/ { start=number_value($0) }
  /"end_line"[[:space:]]*:/ { end=number_value($0) }
  /"mccabe"[[:space:]]*:/ { complexity=number_value($0) }
  /^[[:space:]]*},?[[:space:]]*$/ && file != "" {
    print file "\t" name "\t" start "\t" end "\t" complexity
    file=name=start=end=complexity=""
  }
' "$work_dir/complexity.json" >"$work_dir/functions.raw.tsv"
[[ -s $work_dir/functions.raw.tsv ]] || die "Knots JSON contains no named functions"

awk -F '\t' '{print $1}' "$work_dir/functions.raw.tsv" | sort -u >"$work_dir/files"
: >"$work_dir/path-map.tsv"
while IFS= read -r file; do
  canonical=$(workspace_path "$file") || die "Knots path is outside the workspace: $file"
  printf '%s\t%s\n' "$file" "$canonical" >>"$work_dir/path-map.tsv"
done <"$work_dir/files"

awk -F '\t' -v OFS='\t' '
  NR == FNR { path[$1]=$2; next }
  !($1 in path) || $2 !~ /^[A-Za-z_][A-Za-z0-9_]*$/ ||
      $3 !~ /^[0-9]+$/ || $4 !~ /^[0-9]+$/ || $5 !~ /^[0-9]+$/ ||
      $3 < 1 || $4 < $3 || $5 < 1 { bad=1; next }
  { print path[$1], $2, $3, $4, $5 }
  END { if (bad) exit 2 }
' "$work_dir/path-map.tsv" "$work_dir/functions.raw.tsv" >"$work_dir/functions.mapped.tsv" ||
  die "invalid Knots function record"

awk -F '\t' -v OFS='\t' '
  {
    key=$1 FS $2 FS $3 FS $4
    if (key in complexity && complexity[key] != $5) bad=1
    if (!(key in complexity)) print
    complexity[key]=$5
  }
  END { if (bad) exit 2 }
' "$work_dir/functions.mapped.tsv" >"$work_dir/functions.tsv" ||
  die "conflicting Knots function records"

# Mark direct test functions and conventional inline test modules.
: >"$work_dir/test-scope.tsv"
awk -F '\t' '{print $1}' "$work_dir/functions.tsv" | sort -u |
while IFS= read -r file; do
  awk -v file="$file" '
    function is_test(attribute) {
      return attribute ~ /#\[[[:space:]]*([A-Za-z_][A-Za-z0-9_]*::)*test([^A-Za-z0-9_]|$)/ ||
        (attribute ~ /#\[[[:space:]]*cfg[[:space:]]*\(/ &&
         attribute ~ /(^|[^A-Za-z0-9_-])test([^A-Za-z0-9_-]|$)/)
    }
    {
      line=$0
      sub(/^[[:space:]]*/, "", line)
      if (in_attribute) {
        attribute=attribute " " line
        if (line ~ /]/) in_attribute=0
        next
      }
      if (line ~ /^#\[/) {
        attribute=attribute " " line
        if (line !~ /]/) in_attribute=1
        next
      }
      if (line == "" || line ~ /^\/\//) next
      if (line ~ /(^|[[:space:]])fn[[:space:]]+[A-Za-z_][A-Za-z0-9_]*/) {
        if (is_test(attribute)) print "direct\t" file "\t" NR
        attribute=""
        next
      }
      if (line ~ /^(pub(\([^)]*\))?[[:space:]]+)?mod[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\{/) {
        name=line
        sub(/^.*mod[[:space:]]+/, "", name)
        sub(/[[:space:]]*\{.*$/, "", name)
        if (name == "tests" || is_test(attribute)) {
          print "module\t" file "\t" NR
          exit
        }
      }
      attribute=""
    }
  ' "$root/$file"
done >"$work_dir/test-scope.tsv"

# Join function spans to canonical coverage, excluding nested named functions.
{
  echo '@coverage'
  cat "$work_dir/coverage.tsv"
  echo '@functions'
  cat "$work_dir/functions.tsv"
  echo '@scope'
  cat "$work_dir/test-scope.tsv"
} | awk -F '\t' -v OFS='\t' '
  $0 == "@coverage" { mode="coverage"; next }
  $0 == "@functions" { mode="functions"; next }
  $0 == "@scope" { mode="scope"; next }
  mode == "coverage" { cf[++cn]=$1; cl[cn]=$2; ch[cn]=$3; next }
  mode == "functions" {
    ff[++fn]=$1; name[fn]=$2; first[fn]=$3; last[fn]=$4; cc[fn]=$5
    next
  }
  mode == "scope" {
    if ($1 == "module") module_start[$2]=$3
    else direct[$2 SUBSEP $3]=1
  }
  END {
    for (i=1; i<=fn; i++) {
      executable=covered=0
      for (j=1; j<=cn; j++) {
        if (cf[j] != ff[i] || cl[j] < first[i] || cl[j] > last[i]) continue
        nested=0
        for (k=1; k<=fn; k++) {
          if (k != i && ff[k] == ff[i] && first[k] >= first[i] &&
              last[k] <= last[i] && (first[k] != first[i] || last[k] != last[i]) &&
              cl[j] >= first[k] && cl[j] <= last[k]) { nested=1; break }
        }
        if (!nested) { executable++; covered += ch[j] > 0 }
      }
      scope="production"
      if (ff[i] ~ /(^|\/)tests(\/|$)/ || ff[i] ~ /(^|\/)tests\.rs$/ ||
          ff[i] ~ /(^|\/)test_support\.rs$/ ||
          direct[ff[i] SUBSEP first[i]] ||
          (ff[i] in module_start && first[i] >= module_start[ff[i]])) scope="test"
      else if (ff[i] == "build.rs" || ff[i] ~ /^build_support\//) scope="build"

      if (executable) {
        coverage=covered/executable
        score=cc[i]^2 * (1-coverage)^3 + cc[i]
        printf "1\t%.6f\t%d\t%s\t%d\t%d\t%s\t%s\t%d\t%d\t%.12g\t%.2f\t%.6f\tmeasured\n",
          score, cc[i], ff[i], first[i], last[i], name[i], scope,
          executable, covered, coverage, 100*coverage, score
      } else {
        printf "0\t-1\t%d\t%s\t%d\t%d\t%s\t%s\t0\t0\tnull\tnull\tnull\tzero_instrumented_lines\n",
          cc[i], ff[i], first[i], last[i], name[i], scope
      }
    }
  }
' >"$work_dir/scores.unsorted.tsv"

sort -t $'\t' -k1,1nr -k2,2nr -k3,3nr -k4,4r -k5,5nr -k6,6nr -k7,7r \
  "$work_dir/scores.unsorted.tsv" >"$work_dir/scores.tsv"

{
  echo '@coverage'
  cat "$work_dir/coverage.tsv"
  echo '@scores'
  cat "$work_dir/scores.tsv"
} | awk -F '\t' -v threshold="$threshold" '
  $0 == "@coverage" { mode="coverage"; next }
  $0 == "@scores" { mode="scores"; next }
  mode == "coverage" {
    lines++; covered += $3 > 0
    if (!seen[$1]++) files++
    next
  }
  { named++; measured += $14 == "measured"; zero += $14 != "measured";
    violations += $14 == "measured" && $13 >= threshold }
  END { print files+0 "\t" lines+0 "\t" covered+0 "\t" named+0 "\t" measured+0 "\t" zero+0 "\t" violations+0 }
' >"$work_dir/summary.tsv"
IFS=$'\t' read -r canonical_files executable_lines covered_lines named measured zero violations \
  <"$work_dir/summary.tsv"
coverage_percent=$(awk -v covered="$covered_lines" -v total="$executable_lines" \
  'BEGIN { printf "%.2f", total ? 100*covered/total : 0 }')

{
  printf '{\n  "metadata": {\n'
  printf '    "formula": "C^2 * (1 - p)^3 + C",\n'
  printf '    "review_threshold": %d,\n' "$threshold"
  printf '    "coverage_tool": "cargo-llvm-cov 0.8.5",\n'
  printf '    "coverage_command": "cargo llvm-cov --all-targets --all-features --include-build-script --locked --offline --lcov",\n'
  printf '    "complexity_tool": "knots 1.16.0",\n'
  printf '    "complexity_command": "knots --recursive --language rust --count-anonymous-closures --format json src tests build.rs build_support",\n'
  printf '    "lcov_raw_workspace_record_count": %d,\n' "$raw_records"
  printf '    "lcov_canonical_file_count": %d,\n' "$canonical_files"
  printf '    "lcov_executable_lines": %d,\n' "$executable_lines"
  printf '    "lcov_covered_lines": %d,\n' "$covered_lines"
  printf '    "lcov_coverage_percent": %s,\n' "$coverage_percent"
  printf '    "named_function_count": %d,\n' "$named"
  printf '    "measured_function_count": %d,\n' "$measured"
  printf '    "zero_instrumented_function_count": %d\n' "$zero"
  printf '  },\n  "functions": [\n'
  awk -F '\t' '
    function escape(value) {
      gsub(/\\/, "\\\\", value)
      gsub(/"/, "\\\"", value)
      return value
    }
    {
      if (NR > 1) printf ",\n"
      printf "    {\"scope\": \"%s\", \"file\": \"%s\", \"function\": \"%s\", ",
        $8, escape($4), escape($7)
      printf "\"start_line\": %d, \"end_line\": %d, ", $5, $6
      printf "\"cyclomatic_complexity\": %d, \"executable_lines\": %d, ", $3, $9
      printf "\"covered_lines\": %d, \"coverage_fraction\": %s, ", $10, $11
      printf "\"coverage_percent\": %s, \"crap\": %s, \"status\": \"%s\"}",
        $12, $13, $14
    }
  ' "$work_dir/scores.tsv"
  printf '\n  ]\n}\n'
} >"$work_dir/crap.json"

{
  printf '# CRAP report\n\n'
  printf 'Formula: `C^2 * (1 - p)^3 + C`. Every measured score must be below %d.\n\n' "$threshold"
  printf 'Measured %d of %d named functions; %d had zero instrumented lines. ' "$measured" "$named" "$zero"
  printf '%d measured functions violate the budget.\n\n' "$violations"
  printf 'Canonical coverage: %d/%d (%s%%) across %d files. ' \
    "$covered_lines" "$executable_lines" "$coverage_percent" "$canonical_files"
  printf 'The input contained %d workspace source records.\n\n' "$raw_records"
  printf 'Canonical coverage merges duplicate compilation contexts by resolved source path. '
  printf 'Zero-instrumented functions remain unscored.\n'
  for scope in production build test; do
    printf '\n## Highest %s scores\n\n' "$scope"
    printf '| CRAP | Cyclomatic | Coverage | Function |\n'
    printf '| ---: | ---: | ---: | :--- |\n'
    limit=15
    [[ $scope == production ]] && limit=30
    awk -F '\t' -v scope="$scope" -v limit="$limit" '
      $8 == scope && $14 == "measured" && shown++ < limit {
        printf "| %.2f | %d | %.2f%% | `%s:%d` `%s` |\n", $13, $3, $12, $4, $5, $7
      }
    ' "$work_dir/scores.tsv"
  done
} >"$work_dir/crap.md"

mkdir -p "$output_dir"
for artifact in coverage.lcov complexity.json crap.json crap.md; do
  mv -f "$work_dir/$artifact" "$output_dir/$artifact"
done
echo "CRAP report: $output_dir/crap.md"
if ((violations)); then
  echo "error: $violations measured functions have CRAP scores of at least $threshold" >&2
  exit 1
fi
