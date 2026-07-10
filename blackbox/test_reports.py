from conftest import run_duh


def test_top_lists_biggest_dirs(scanned):
    out = run_duh("top", "--under", scanned.root, "-d", "1", db=scanned.db).stdout
    assert "clones" in out and "siblings" in out


def test_clones_lists_family(scanned):
    out = run_duh("clones", db=scanned.db).stdout
    assert "big.bin" in out or "a.bin" in out


def test_clusters_finds_sibling_group(scanned):
    out = run_duh("clusters", "--min-bytes", str(1 << 20), db=scanned.db).stdout
    assert "siblings" in out


def test_excluded_lists_node_modules(scanned):
    out = run_duh("excluded", db=scanned.db).stdout
    assert "node_modules" in out


def test_stats_runs(scanned):
    assert run_duh("stats", db=scanned.db).returncode == 0

