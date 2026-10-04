#!/usr/bin/env bash
# The controlled comparison R-011 asks for: what is the module table worth, and through which rung?
#
# Both arms come out of **one build**, selected by `PEEK_MODULE_TABLE`. That is the whole reason
# the switch exists. The previous attempt to answer this by running the probe on two consecutive
# revisions produced nothing but two measurement errors (R-010), one of which - a probe that deleted
# a file from the repository it was measuring - manufactured a convincing false trend.
#
# So this script is deliberately dull about the things that went wrong:
#
#   * **One build, two arms.** `PEEK_MODULE_TABLE=0` and `=1`, same binary.
#   * **One clone set, reused.** Both arms see identical source. No re-cloning between arms, so
#     no arm can be measuring a different tree.
#   * **A clone that is checked before it is reused.** A probe killed between deleting a file and
#     putting it back leaves the clone smaller than the next run expects. That is R-010 again, so a
#     reused clone whose fingerprint no longer matches is re-cloned rather than measured.
#   * **A fresh index per repository per arm.** A shared index would let the second arm read the
#     first arm's rows, and the per-rung counts below are read straight out of that database.
#   * **No `sed` gymnastics on the numbers.** Each arm's state figures are extracted with the same
#     grep the probe itself prints, and the per-rung figures come from the index the probe wrote.
#   * **It states whether the arms differ at all.** If they are byte-identical, the switch is
#     broken or the table never fires, and the script says so rather than printing two equal rows
#     and letting the reader conclude "no effect".
#
# The metric, and why the previous one could not see it
#
# **resolved share = resolved / relations.** The share of relations the index can *prove*, over
# the whole graph. Not a rate over a subset of the states, and not the decided rate.
#
# The first version of this script reported `(resolved + inferred) / relations` - the decided
# rate - and that is the wrong shape for the question. The module table's work is largely to
# turn `inferred` into `resolved`: the same edge, reached by an import binding or a located
# module rather than by a repository-wide uniqueness check. That is movement *inside* decided, so
# a rate over the decided pair is blind to it by construction, and the arms came out within
# 0.4 percentage points of each other on five repositories while the table was demonstrably
# firing. Counting `Resolved` on its own puts the proof and the claim in different numerators, so
# the movement is visible.
#
# The per-rung breakdown, and why it needs no engine change
#
# A relation records *why* it was decided. `ResolutionState::Resolved { by }` and
# `Inferred { by }` carry an `Evidence`, it is written into `resolution_json` when the row is
# stored, and `resolve::rung_name` maps an evidence class onto the ladder rung that produced it.
# So the rung a relation was resolved by is already in the database, and the breakdown is a
# query over stored state rather than a new column or a new switch. `Evidence::class()` and
# `resolve::rung_name` are not the same vocabulary - a receiver is stored as `receiver_type` and
# is the `receiver_owner` rung - so the mapping is reproduced in the helper below and an
# unrecognised class is *reported*, never dropped.
#
# Both arms index the same clone, so both arms hold the same relations under the same natural
# key. That makes the comparison a join rather than a subtraction, and the join is what answers
# the question: two per-arm totals say a number moved, and the transitions say *which rung
# vacated it and which rung took it*. A single number per repository is what made this
# uninterpretable twice.
#
# The attribution, and the tests that could falsify it
#
# `PEEK_MODULE_TABLE` gates `Resolver::module_files`, which has exactly two call sites: R1
# through `targets_of`, and R3. R1 reports `Evidence::ImportBinding` and R3 reports
# `Evidence::QualifiedNameInScope`, so the table can only ever change the answer of a relation
# into `import_binding` or `scope_qualified_name`. Everything else it does is displacement: a
# relation the table now answers never reaches the rung that used to answer it.
#
# That is a reading of the call graph, and a reading of code is a hypothesis. So it is stated as
# five predictions that a run can fail, and the run checks all five:
#
#   P1  the receiver rung decides the same number of relations on both arms - it is consulted
#       before any gated rung and reads nothing the switch touches;
#   P2  no relation gains a decision in a rung outside the two gated ones;
#   P3  no relation loses a decision from a rung outside the two gated ones;
#   P4  no rung that does not read the table gained relations;
#   P5  no relation resolved by an ungated rung became `ambiguous` - the ungated rungs are only
#       reached once the gated ones decline, and they answer the same either way.
#
# All five holding is the attribution. One failing means the mechanism above is incomplete, and
# the run names which.
#
# What this still cannot say, and is said here rather than discovered later
#
#   * **`import_binding` is two rungs.** R1 and the imported-receiver fall-through both report
#     `Evidence::ImportBinding`, because the author's `use` is the honest description of how the
#     target was found in both cases. The store cannot separate them; the receiver evidence is
#     replaced by the binding when the decision is written. Splitting them needs a stored marker
#     or a third switch, which is an engine contract change and is not made here.
#   * **`ambiguous` carries no evidence.** A row in that state names candidates and no class, so
#     the rung that *produced* the ambiguity is not recorded. The counts below can say how many
#     relations became ambiguous and which rung they had before; they cannot say which rung made
#     them so.
#   * **Nothing here says the table is a good idea.** It says where its effect comes from. The
#     direction and the size are separate questions and both are reported.
#
# Reading the index needs `python3` and its standard-library `sqlite3`. The verification template
# carries no `sqlite3` binary, and a measurement script that quietly skipped the rung breakdown
# because a tool was missing would be the same defect as one that prints two equal rows.
#
# Usage, inside the verification sandbox:
#
#   PEEK_PROBE_WORK=/tmp/probe bash scripts/ab-module-table.sh
#   PEEK_PROBE_WORK=/tmp/probe bash scripts/ab-module-table.sh rust-lang/regex   # just one
#
# Exit code is 0 when both arms completed and were read for every repository, whatever the
# deltas are and whatever the mechanism checks say. It is not a pass/fail test: this is a
# measurement, and a measurement with a threshold nobody has justified is the defect this project
# exists to remove.

set -uo pipefail

WORK="${PEEK_PROBE_WORK:-/tmp/peek-ab}"
OUT="$WORK/ab"
REPOS="${*:-rust-lang/regex BurntSushi/ripgrep serde-rs/serde tokio-rs/axum rust-lang/cargo}"

mkdir -p "$OUT"

for tool in cargo git python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "missing required tool: $tool" >&2
    exit 2
  fi
done
if ! python3 -c 'import sqlite3' 2>/dev/null; then
  echo "python3 has no sqlite3 module; the per-rung breakdown cannot be read" >&2
  exit 2
fi

field() {
  local line value
  line=$(grep -m1 -E "^$2" "$1" 2>/dev/null) || true
  if [ -z "$line" ]; then
    echo "MISSING($2)" >&2
    echo 0
    return
  fi
  value=$(printf '%s' "$line" | tr -s ' ' | cut -d: -f2- | xargs)
  if ! printf '%s' "$value" | grep -qE '^[0-9]+$'; then
    echo "NOT-A-NUMBER($2): $line" >&2
    echo 0
    return
  fi
  echo "$value"
}

# How many files and how many bytes a clone holds.
#
# This exists because of R-010. `report_deletion` deletes a file and puts it back, and a run
# killed in between leaves the clone one file short; the next run reuses the clone, indexes a
# smaller tree, and the difference is read as a change in the engine. A file count and a byte
# count catch that before a number is produced rather than after one has been believed.
fingerprint() {
  printf '%s/%s\n' \
    "$(find "$1" -type f | wc -l | tr -d ' ')" \
    "$(du -sb "$1" 2>/dev/null | cut -f1)"
}

# The database the probe wrote for one repository under one arm.
#
# `paths::index_dir` is `<root>/<repo-id>` and the file inside it is `index.db`, so
# `PEEK_INDEX_DIR` is the *parent* of the database rather than its own directory.
index_db() {
  find "$1" -type f -name index.db 2>/dev/null | head -1
}

# One repository, one arm. Prints
# `slug entities relations resolved inferred ambiguous unresolved pending`.
run_one() {
  local arm="$1" slug="$2" dir="$3"
  local index="$OUT/index-$arm-$slug"
  rm -rf "$index"
  mkdir -p "$index"
  local log="$OUT/$slug.arm$arm.log"
  if ! PEEK_MODULE_TABLE="$arm" PEEK_PROBE_REPO="$dir" PEEK_INDEX_DIR="$index" \
       cargo test --test real_repository --release -- --ignored --nocapture >"$log" 2>&1; then
    echo "ARM $arm FAILED: $slug (see $log)" >&2
    tail -5 "$log" | sed 's/^/    /' >&2
    return 1
  fi
  printf '%s %s %s %s %s %s %s %s\n' \
    "$slug" \
    "$(field "$log" '^entities:')" \
    "$(field "$log" '^relations:')" \
    "$(field "$log" '^resolved:')" \
    "$(field "$log" '^inferred:')" \
    "$(field "$log" '^ambiguous:')" \
    "$(field "$log" '^unresolved:')" \
    "$(field "$log" '^pending:')"
}

# The per-rung reader. Written to the work directory rather than kept in this file so that a run
# leaves the exact analysis behind, and so that the analysis is a thing a reader can open.
RUNG_TOOL="$OUT/rungs.py"
cat >"$RUNG_TOOL" <<'PYTHON'
#!/usr/bin/env python3
"""Per-rung attribution of the module table, read from two indexes and nothing else.

Joins the off-arm and on-arm databases of one repository on the natural key the schema declares,
and reports which rung decided each relation in each arm, what moved between them, and whether
that movement is consistent with the table being the only thing that changed.
"""
import collections
import json
import sqlite3
import sys

# The natural key of a relation, in the order the schema's UNIQUE constraint lists it. The
# surrogate `id` is deliberately not used: it is a row number, and two indexes built from the same
# source need not agree on it, while these twelve columns are the fact itself.
KEY = ("source_path", "source_kind", "source_qualified_name", "source_ordinal",
       "kind", "target_name", "start_byte", "end_byte", "start_line",
       "start_column", "end_line", "end_column")

# `resolve::rung_name`, reproduced. The stored text is `Evidence::class()`, which is a different
# vocabulary: a receiver is stored as `receiver_type` and is the receiver-owner rung. A class this
# table has never seen is reported under its own name rather than folded into a bucket, because a
# rung that cannot be named is a hole and a hole should be visible.
RUNG = {
    "containment": "containment",
    "import_binding": "import_binding",
    "receiver_type": "receiver_owner",
    "path_match": "path_match",
    "qualified_name_in_scope": "scope_qualified_name",
    "same_file": "same_file",
    "unique_name": "unique_name",
    "name_only": "name_only",
}

# The rungs whose lookup the switch turns off, in the vocabulary of the evidence classes they
# write. `Resolver::module_files` has two call sites - R1 through `targets_of`, and R3 - and R1
# reports `import_binding` while R3 reports `qualified_name_in_scope`.
GATED = ("import_binding", "scope_qualified_name")

DECIDED = ("resolved", "inferred")
UNPLACED = "ABSENT"
TOP = 8


def label_of(state, payload, notes):
    """`state:rung`, from the stored tag and the stored payload."""
    if state == "ambiguous":
        # Deliberately classless: `ResolutionState::Ambiguous` carries candidates and nothing else,
        # so the rung that produced the ambiguity is not in the index and cannot be recovered.
        return "ambiguous"
    try:
        stored = json.loads(payload)
    except ValueError:
        notes.add("payload-unreadable")
        return "%s:UNREADABLE" % state
    if not isinstance(stored, dict):
        notes.add("payload-not-an-object")
        return "%s:UNREADABLE" % state
    if state == "unresolved":
        return "unresolved:%s" % stored.get("reason", "?")
    # `by` for the two decided states, `evidence` for `pending`.
    holder = stored.get("by") if state in DECIDED else stored.get("evidence")
    if not isinstance(holder, dict) or "class" not in holder:
        notes.add("%s-with-no-class" % state)
        return "%s:NO-CLASS" % state
    klass = holder["class"]
    rung = RUNG.get(klass)
    if rung is None:
        notes.add("class-%s" % klass)
        return "%s:UNMAPPED(%s)" % (state, klass)
    return "%s:%s" % (state, rung)


def read_labels(db_path, notes):
    """state:rung for every relation in one index, keyed by the natural key."""
    con = sqlite3.connect(db_path)
    try:
        sql = "SELECT %s, resolution_state, resolution_json FROM relation" % ", ".join(KEY)
        labels = {}
        for row in con.execute(sql):
            labels[tuple(row[:len(KEY)])] = label_of(row[len(KEY)], row[len(KEY) + 1], notes)
        return labels, con.execute("SELECT COUNT(*) FROM relation").fetchone()[0]
    finally:
        con.close()


def state_of(label):
    return label.split(":", 1)[0]


def rung_of(label):
    parts = label.split(":", 1)
    return parts[1] if len(parts) == 2 else ""


def signed(value):
    return "%+d" % value


def thousands(value):
    return "{:,}".format(value)


UNDECIDED = ("pending", "ambiguous", "unresolved")


def bucket_of(off, on):
    """Which of six mutually exclusive movements this pair is.

    Every pair that differs falls in exactly one, so the categories sum to the number of relations
    whose decision is not the same on both arms and no relation is counted twice. They are cut on
    the two states that carry the metric - was there a proof, and is there one now - rather than on
    "was there any decision at all", because `resolved -> inferred` is a change of the thing being
    measured and a bucket that scored it zero would make the net disagree with the totals.
    """
    a, b = state_of(off), state_of(on)
    if a == "resolved":
        return "proof rerouted" if b == "resolved" else "lost its proof"
    if b == "resolved":
        return "gained a proof"
    if a == "inferred":
        return "claim rerouted" if b == "inferred" else "nothing gained but a claim"
    if b == "inferred":
        return "nothing gained but a claim"
    return "moved among the undecided"


def share_effect(bucket, count):
    """What this movement is worth in the resolved count, which is what the share is made of."""
    if bucket == "gained a proof":
        return count
    if bucket == "lost its proof":
        return -count
    return 0


ORDER = ["gained a proof", "lost its proof", "proof rerouted", "claim rerouted",
         "nothing gained but a claim", "moved among the undecided"]


def main():
    (slug, off_db, on_db, pairs_path, summary_path,
     o_res, o_inf, o_amb, o_unr, o_pen, o_rel,
     n_res, n_inf, n_amb, n_unr, n_pen, n_rel) = sys.argv[1:18]
    logged = {"off": [int(v) for v in (o_res, o_inf, o_amb, o_unr, o_pen, o_rel)],
              "on": [int(v) for v in (n_res, n_inf, n_amb, n_unr, n_pen, n_rel)]}

    notes = {"off": set(), "on": set()}
    off, off_total = read_labels(off_db, notes["off"])
    on, on_total = read_labels(on_db, notes["on"])

    # The join. Every relation of one arm is paired with the same relation of the other, or
    # recorded as present on one side only - which would itself be a finding, because the switch
    # changes a lookup and not the extractor.
    pairs = collections.Counter()
    only = {"off": 0, "on": 0}
    for key, lab in off.items():
        other = on.pop(key, None)
        if other is None:
            only["off"] += 1
            pairs[(lab, UNPLACED)] += 1
        else:
            pairs[(lab, other)] += 1
    for key, lab in on.items():
        only["on"] += 1
        pairs[(UNPLACED, lab)] += 1

    with open(pairs_path, "w", encoding="utf-8") as handle:
        handle.write("# off_label\ton_label\tcount\n")
        for (a, b), count in sorted(pairs.items(), key=lambda kv: (-kv[1], kv[0])):
            handle.write("%s\t%s\t%d\n" % (a, b, count))

    # Per-arm marginals: rung within each decided state, and the reason within `unresolved`.
    by_rung = {"off": collections.Counter(), "on": collections.Counter()}
    by_reason = {"off": collections.Counter(), "on": collections.Counter()}
    ambiguous = {"off": 0, "on": 0}
    for (a, b), count in pairs.items():
        for side, lab in (("off", a), ("on", b)):
            if lab == UNPLACED:
                continue
            state = state_of(lab)
            if state in ("resolved", "inferred", "pending"):
                by_rung[side]["%s:%s" % (state, rung_of(lab) or "NO-RUNG")] += count
            elif state == "ambiguous":
                ambiguous[side] += count
            else:
                by_reason[side][rung_of(lab) or "NO-REASON"] += count

    def rungs(side, state):
        prefix = state + ":"
        return {lab[len(prefix):]: n for lab, n in by_rung[side].items()
                if lab.startswith(prefix)}

    # The join's output, bucketed.
    grouped = collections.defaultdict(collections.Counter)
    for (a, b), count in pairs.items():
        if a == UNPLACED or b == UNPLACED or a == b:
            continue
        grouped[bucket_of(a, b)][(a, b)] += count

    # The predictions the call graph makes, each of which a run can fail.
    checks = []

    receiver = {side: rungs(side, "resolved").get("receiver_owner", 0)
                + rungs(side, "inferred").get("receiver_owner", 0) for side in ("off", "on")}
    checks.append((
        "P1 receiver rung decides the same number on both arms",
        receiver["off"] != receiver["on"],
        "off %s, on %s" % (thousands(receiver["off"]), thousands(receiver["on"])),
    ))

    ungained = sum(count for (a, b), count in grouped["gained a proof"].items()
                   if rung_of(b) not in GATED)
    ungained += sum(count for (a, b), count in grouped["nothing gained but a claim"].items()
                    if rung_of(b) not in GATED)
    checks.append((
        "P2 no relation gained a decision in a rung the switch does not gate",
        ungained != 0, "%s relation(s)" % thousands(ungained),
    ))

    unlost = sum(count for (a, b), count in grouped["lost its proof"].items()
                 if rung_of(a) not in GATED)
    checks.append((
        "P3 no relation lost a proof from a rung the switch does not gate",
        unlost != 0, "%s relation(s)" % thousands(unlost),
    ))

    rose = []
    for state in ("resolved", "inferred"):
        left, right = rungs("off", state), rungs("on", state)
        for rung in set(left) | set(right):
            delta = right.get(rung, 0) - left.get(rung, 0)
            if rung not in GATED and delta > 0:
                rose.append("%s %s %s" % (state, rung, signed(delta)))
    checks.append((
        "P4 no rung the switch does not gate gained relations",
        bool(rose), ", ".join(sorted(rose)) or "none",
    ))

    stranded = sum(count for (a, b), count in grouped["lost its proof"].items()
                   if state_of(b) == "ambiguous" and rung_of(a) not in GATED)
    checks.append((
        "P5 nothing resolved by an ungated rung became ambiguous",
        stranded != 0, "%s relation(s)" % thousands(stranded),
    ))
    failed = [name for name, bad, _ in checks if bad]

    # --- report -------------------------------------------------------------
    out = ["== %s" % slug,
           "   %s relations in the off index and %s in the on index; %s present on one arm only"
           % (thousands(off_total), thousands(on_total),
              thousands(only["off"] + only["on"]))]

    shares = {}
    for side, index_total in (("off", off_total), ("on", on_total)):
        logged_states = logged[side]
        index_states = [sum(rungs(side, "resolved").values()),
                        sum(rungs(side, "inferred").values()),
                        ambiguous[side],
                        sum(by_reason[side].values()),
                        sum(rungs(side, "pending").values())]
        if logged_states[:5] != index_states:
            out.append("   STATE COUNTS DIFFER on the %s arm: the probe log says %s and the index "
                       "this run read says %s. The log is printed after the full build; the index "
                       "has since been through a refresh and a delete-and-restore, so a difference "
                       "here means the figures below are not the figures above."
                       % (side, logged_states[:5], index_states))
        out.append("   %s arm: resolved %s, inferred %s, ambiguous %s, unresolved %s, pending %s, "
                   "which sum to %s of %s relations"
                   % ((side,) + tuple(thousands(v) for v in index_states)
                      + (thousands(sum(index_states)), thousands(index_total))))
        if sum(index_states) != index_total:
            out.append("   HOLE on the %s arm: a relation is in none of the five states, so the "
                       "partition the probe asserts does not hold for the index that was read."
                       % side)
        shares[side] = 100.0 * index_states[0] / index_total if index_total else 0.0

    out.append("   resolved share %.1f%% -> %.1f%%  (%+.1fpp)"
               % (shares["off"], shares["on"], shares["on"] - shares["off"]))

    def table(state, title):
        left, right = rungs("off", state), rungs("on", state)
        if not left and not right:
            return
        rows = sorted(((rung, left.get(rung, 0), right.get(rung, 0)) for rung in set(left) | set(right)),
                      key=lambda row: (-abs(row[2] - row[1]), row[0]))
        out.append("")
        out.append("   %-30s %9s %9s %9s" % (title, "off", "on", "delta"))
        for rung, a, b in rows:
            out.append("     %-28s %9s %9s %9s%s"
                       % (rung, thousands(a), thousands(b), signed(b - a),
                          "  <- reads the table" if rung in GATED else ""))
        out.append("     %-28s %9s %9s %9s"
                   % ("TOTAL", thousands(sum(r[1] for r in rows)),
                      thousands(sum(r[2] for r in rows)),
                      signed(sum(r[2] - r[1] for r in rows))))

    table("resolved", "resolved, by rung")
    table("inferred", "inferred, by rung")
    table("pending", "pending, by rung")
    for side in ("off", "on"):
        if by_reason[side]:
            out.append("")
            out.append("   unresolved, by reason (%s arm)" % side)
            for reason, count in sorted(by_reason[side].items(), key=lambda r: -r[1]):
                out.append("     %-28s %9s" % (reason, thousands(count)))

    out.append("")
    out.append("   movement, by category; a relation is in exactly one")
    out.append("     %-42s %9s %11s" % ("off -> on", "relations", "share effect"))
    net = 0
    for bucket in ORDER:
        rows = sorted(grouped[bucket].items(), key=lambda kv: (-kv[1], kv[0]))
        if not rows:
            continue
        effect = sum(share_effect(bucket, count) for _, count in rows)
        net += effect
        out.append("     %-42s %9s %11s"
                   % (bucket, signed(sum(count for _, count in rows)), signed(effect)))
        for (a, b), count in rows[:TOP]:
            out.append("       %-40s %9s %11s"
                       % ("%s -> %s" % (a, b), signed(count),
                          signed(share_effect(bucket, count))))
        if len(rows) > TOP:
            rest = rows[TOP:]
            out.append("       %-40s %9s %11s"
                       % ("%d more transitions, in %s" % (len(rest), pairs_path),
                          signed(sum(count for _, count in rest)),
                          signed(sum(share_effect(bucket, count) for _, count in rest))))
    out.append("     %-42s %9s %11s" % ("net effect on the resolved count", "", signed(net)))
    # The decomposition is only worth printing if it adds up to the totals it claims to explain.
    # A net that disagrees with the difference of the two `resolved` totals would mean the
    # categories are not the partition they are documented as being, and the whole table would be
    # arithmetic dressed up as a measurement.
    delta_resolved = sum(rungs("on", "resolved").values()) - sum(rungs("off", "resolved").values())
    adds_up = net == delta_resolved
    out.append("     %-42s %9s %11s  %s"
               % ("resolved totals differ by", "", signed(delta_resolved),
                  "a partition, so the net is real" if adds_up else
                  "DOES NOT MATCH THE NET - %s relation(s) are on one arm only and so have no "
                  "transition, or a state outside the five" % (only["off"] + only["on"])))

    out.append("")
    out.append("   the switch gates %s, and only those; a movement in any other rung is the table"
               % " and ".join(GATED))
    out.append("   displacing that rung rather than the table deciding more edges")
    for name, bad, detail in checks:
        out.append("     [%s] %-64s %s" % ("FAIL" if bad else "ok", name, detail))
    conditions_failed = len(failed) + (0 if adds_up else 1)
    if conditions_failed:
        out.append("     %d of %d conditions failed, so the mechanism above does not describe"
                   % (conditions_failed, len(checks) + 1))
        out.append("     this run and the attribution is not established")

    for side in ("off", "on"):
        if notes[side]:
            out.append("   %s arm, stored classes this build cannot name: %s"
                       % (side, ", ".join(sorted(notes[side]))))
        else:
            out.append("   %s arm: every stored evidence class is one this build knows" % side)

    moved = {}
    for state in ("resolved", "inferred", "pending"):
        left, right = rungs("off", state), rungs("on", state)
        for rung in GATED:
            delta = right.get(rung, 0) - left.get(rung, 0)
            if delta:
                moved["%s:%s" % (state, rung)] = delta
    top = ", ".join("%s %s" % (label, signed(delta))
                    for label, delta in sorted(moved.items(), key=lambda kv: -abs(kv[1]))) or \
        "neither gated rung moved"
    with open(summary_path, "w", encoding="utf-8") as handle:
        handle.write("\t".join([
            slug, str(logged["off"][5]), "%.1f" % shares["off"], "%.1f" % shares["on"],
            "%+.1f" % (shares["on"] - shares["off"]), signed(net), top,
            "failed %d/%d" % (conditions_failed, len(checks) + 1),
            str(only["off"] + only["on"]),
        ]) + "\n")

    print("\n".join(out))
    return 1 if (conditions_failed or only["off"] or only["on"]) else 0


if __name__ == "__main__":
    sys.exit(main())
PYTHON

printf '%-22s %10s %9s %9s %9s  %s\n' \
  'repository' 'relations' 'share off' 'share on' 'delta' 'gated rungs that moved'
printf -- '---------------------------------------------------------------------------------\n'

failures=0
for slug in $REPOS; do
  safe="${slug//\//__}"
  dir="$WORK/repos/$safe"

  if [ -d "$dir/.git" ]; then
    # R-010 guard. A clone is reused only if it is the size it was when it was fingerprinted; if a
    # probe was killed mid-deletion the clone is short, and measuring it would report the tool's
    # own damage as a property of the engine.
    want=$(cat "$OUT/$safe.fingerprint" 2>/dev/null || echo '')
    have=$(fingerprint "$dir")
    if [ "$want" != "$have" ]; then
      echo "recloning $slug: the clone is not the one that was fingerprinted (now $have, then $want)"
      rm -rf "$dir"
    fi
  fi

  if [ ! -d "$dir/.git" ]; then
    echo "cloning $slug ..."
    mkdir -p "$WORK/repos"
    if ! git clone --depth 1 --quiet "https://github.com/$slug.git" "$dir" 2>"$OUT/$safe.clone.err"; then
      echo "CLONE FAILED: $slug" >&2
      sed 's/^/    /' "$OUT/$safe.clone.err" | head -3 >&2
      failures=$((failures + 1))
      continue
    fi
    fingerprint "$dir" >"$OUT/$safe.fingerprint"
  fi

  off=$(run_one 0 "$safe" "$dir") || { failures=$((failures + 1)); continue; }
  on=$(run_one 1 "$safe" "$dir") || { failures=$((failures + 1)); continue; }

  # `run_one` prints `slug entities relations resolved inferred ambiguous unresolved pending`, so
  # the slug occupies `$1` and the first *figure* is `$2`. Getting this off by one is not a
  # cosmetic slip: it makes `relations` read back as the entity count, which produces a
  # denominator that is not the graph. The partition assertion below is what caught it, and it is
  # the reason that assertion is here at all.
  # shellcheck disable=SC2086
  set -- $off
  o_ent=$2; o_rel=$3; o_res=$4; o_inf=$5; o_amb=$6; o_unr=$7; o_pen=$8
  # shellcheck disable=SC2086
  set -- $on
  n_ent=$2; n_rel=$3; n_res=$4; n_inf=$5; n_amb=$6; n_unr=$7; n_pen=$8

  # The states must partition on both arms, over all five the probe counts, and the partition is
  # asserted rather than assumed because a rate computed over a subset of them is a share of
  # something that is not the graph. `pending` is in the sum because the probe asserts it in its
  # own right: leaving it out is what let a stale index pass as a measured one.
  o_total=$((o_res + o_inf + o_amb + o_unr + o_pen))
  n_total=$((n_res + n_inf + n_amb + n_unr + n_pen))
  note=""
  if [ "$o_total" -ne "$o_rel" ] 2>/dev/null; then
    note="off-arm states do not partition ($o_total vs $o_rel)"
  fi
  if [ "$n_total" -ne "$n_rel" ] 2>/dev/null; then
    note="$note; on-arm states do not partition ($n_total vs $n_rel)"
  fi
  # Compared across the whole state vector, not just the two the rate is built from: a difference
  # hidden in `inferred` is the whole point of the metric, so an equality check that ignored it
  # would call two different indexes identical.
  if [ "$o_ent" = "$n_ent" ] && [ "$o_rel" = "$n_rel" ] && [ "$o_res" = "$n_res" ] && \
     [ "$o_inf" = "$n_inf" ] && [ "$o_amb" = "$n_amb" ] && [ "$o_unr" = "$n_unr" ]; then
    note="$note ARMS ARE IDENTICAL - the switch may be doing nothing, or the table never fires"
  fi
  [ -n "$note" ] && echo "  $safe: $note"

  off_db=$(index_db "$OUT/index-0-$safe")
  on_db=$(index_db "$OUT/index-1-$safe")
  if [ -z "$off_db" ] || [ -z "$on_db" ]; then
    echo "  $safe: no index.db for one of the arms, so the per-rung breakdown is unavailable" >&2
    failures=$((failures + 1))
    continue
  fi

  python3 "$RUNG_TOOL" \
    "$safe" "$off_db" "$on_db" \
    "$OUT/$safe.pairs" "$OUT/$safe.summary" \
    "$o_res" "$o_inf" "$o_amb" "$o_unr" "$o_pen" "$o_rel" \
    "$n_res" "$n_inf" "$n_amb" "$n_unr" "$n_pen" "$n_rel"
  read_status=$?
  if [ "$read_status" -ne 0 ]; then
    # A non-zero status is information, not a failure of the run: the reader returns 1 when a
    # prediction failed or a relation was on one arm only, and both of those are findings. The
    # status is captured before anything else runs, because reading `$?` again after the test
    # would report the test's own status and print a cheerful zero for a reader that complained.
    echo "  $safe: the rung reader reported a condition it could not explain (exit $read_status)" >&2
  fi
  echo
done

echo
echo '=== summary ==='
printf '%-22s %10s %9s %9s %9s  %-40s %-12s %s\n' \
  'repository' 'relations' 'share off' 'share on' 'delta' 'gated rungs that moved' 'checks' 'unmatched'
printf -- '-------------------------------------------------------------------------------------------------\n'
for slug in $REPOS; do
  safe="${slug//\//__}"
  row="$OUT/$safe.summary"
  if [ ! -f "$row" ]; then
    printf '%-22s  no summary: this repository produced no readable pair of arms\n' "$safe"
    continue
  fi
  IFS=$'\t' read -r name rel share_o share_n delta net top checks unmatched <"$row"
  printf '%-22s %10s %8s%% %8s%% %8s  %-40s %-12s %s\n' \
    "$name" "$rel" "$share_o" "$share_n" "${delta}pp" "$top" "$checks" "$unmatched"
done

echo
echo "resolved share is resolved / relations - the proofs, not the claims"
echo "the five states partition on both arms; decided rate is (resolved + inferred) / relations"
echo "a rung is read from the evidence class stored on the relation, so the breakdown is a query"
echo "over the index the probe wrote and asks the engine for nothing it does not already store"
echo "the switch gates import_binding and scope_qualified_name; movement in any other rung is the"
echo "table displacing that rung, and the five predictions say whether that reading held"
echo "per-transition counts: $OUT/<repository>.pairs"
echo "logs: $OUT"
exit "$failures"