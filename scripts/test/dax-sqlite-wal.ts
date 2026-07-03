// Regression test for the historical claim (commit 37e937aa) that writable
// host-share DAX can wedge SQLite WAL close/unmap paths on macOS. Runs a
// real SQLite WAL workload on a DAX-mounted host share and exercises exactly
// those paths: shm mmap/munmap, wal_checkpoint(TRUNCATE) of a mapped WAL,
// last-close unlink of -wal/-shm, cross-process shm mappings, and close
// after a snapshot-exit restore. A wedge shows up as a guest command timeout.
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { Database } from "bun:sqlite";
import {
  assertContains,
  assertEq,
  cleanupContext,
  defaultContext,
  prepareContext,
  testStep,
} from "./lib";

const ctx = defaultContext("dax-sqlite-wal");
const cwd = join(ctx.repoRoot, ".lnx-dax-sqlite-wal");
const dbName = "wal-wedge.db";
const cycles = 20;
const rowsPerCycle = 200;

async function cleanupDirs() {
  await rm(cwd, { recursive: true, force: true });
}

const pythonPrelude = String.raw`
import os
import sqlite3
import subprocess

mount = subprocess.check_output(["findmnt", "-T", os.getcwd(), "-no", "FSTYPE,OPTIONS"], text=True).strip()
assert mount.startswith("virtiofs ") and "dax=always" in mount, mount

DB = ${JSON.stringify(dbName)}
CYCLES = ${cycles}
ROWS_PER_CYCLE = ${rowsPerCycle}

def open_db():
    conn = sqlite3.connect(DB, timeout=10)
    (mode,) = conn.execute("PRAGMA journal_mode=WAL").fetchone()
    assert mode == "wal", mode
    # Map the main database file too, so close also tears down a DAX mapping
    # of the db itself, not just the -shm index.
    conn.execute("PRAGMA mmap_size=%d" % (64 * 1024 * 1024))
    conn.execute("PRAGMA synchronous=NORMAL")
    return conn
`;

try {
  await prepareContext(ctx);
  await cleanupDirs();
  await mkdir(cwd, { recursive: true });

  await testStep("sqlite WAL open/write/checkpoint/close cycles over DAX host share", async () => {
    const result = await ctx.vm.cli(["python3", "-"], {
      cwd,
      timeoutMs: 180_000,
      stdin:
        pythonPrelude +
        String.raw`
conn = open_db()
conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, cycle INTEGER, seq INTEGER)")
conn.commit()
conn.close()

for cycle in range(CYCLES):
    conn = open_db()
    with conn:
        conn.executemany(
            "INSERT INTO t (cycle, seq) VALUES (?, ?)",
            [(cycle, seq) for seq in range(ROWS_PER_CYCLE)],
        )
    # Truncate-checkpoint ftruncates the WAL while the shm index is mapped.
    busy, log_frames, ckpt_frames = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
    assert busy == 0, (busy, log_frames, ckpt_frames)
    # Last close unmaps the shm and unlinks -wal/-shm. This is the path the
    # krun.rs comment claims can wedge.
    conn.close()
    assert not os.path.exists(DB + "-wal"), cycle
    assert not os.path.exists(DB + "-shm"), cycle
    print("cycle-done %d" % cycle, flush=True)

conn = open_db()
(count,) = conn.execute("SELECT COUNT(*) FROM t").fetchone()
(seq_sum,) = conn.execute("SELECT SUM(seq) FROM t").fetchone()
conn.close()
assert count == CYCLES * ROWS_PER_CYCLE, count
assert seq_sum == CYCLES * ROWS_PER_CYCLE * (ROWS_PER_CYCLE - 1) // 2, seq_sum
print("wal-cycles-ok", flush=True)
`,
    });
    assertContains(result.stdout, `cycle-done ${cycles - 1}`, "all WAL cycles completed");
    assertContains(result.stdout, "wal-cycles-ok", "WAL cycle integrity check passed");
  });

  await testStep("concurrent cross-process WAL readers while writer holds shm mapping", async () => {
    const result = await ctx.vm.cli(["python3", "-"], {
      cwd,
      timeoutMs: 180_000,
      stdin:
        pythonPrelude +
        String.raw`
READER = """
import sqlite3
conn = sqlite3.connect(${JSON.stringify(dbName)}, timeout=10)
(mode,) = conn.execute("PRAGMA journal_mode=WAL").fetchone()
assert mode == "wal", mode
(count,) = conn.execute("SELECT COUNT(*) FROM t").fetchone()
conn.close()
print(count)
"""

writer = open_db()
with writer:
    writer.executemany(
        "INSERT INTO t (cycle, seq) VALUES (?, ?)",
        [(CYCLES, seq) for seq in range(ROWS_PER_CYCLE)],
    )

expected = (CYCLES + 1) * ROWS_PER_CYCLE
# Each reader maps and unmaps its own view of the -shm file while the
# writer's mapping stays live, then the reader's close drops its mapping.
for round in range(5):
    out = subprocess.check_output(["python3", "-c", READER], text=True).strip()
    assert out == str(expected), (round, out)
    print("reader-done %d" % round, flush=True)

busy, _, _ = writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
assert busy == 0
writer.close()
assert not os.path.exists(DB + "-wal")
assert not os.path.exists(DB + "-shm")
print("wal-readers-ok", flush=True)
`,
    });
    assertContains(result.stdout, "reader-done 4", "all concurrent readers completed");
    assertContains(result.stdout, "wal-readers-ok", "writer close after readers succeeded");
  });

  await testStep("WAL close/unmap after snapshot-exit restore", async () => {
    const result = await ctx.vm.cli(["python3", "-"], {
      cwd,
      timeoutMs: 180_000,
      stdin:
        pythonPrelude +
        String.raw`
conn = open_db()
with conn:
    conn.executemany(
        "INSERT INTO t (cycle, seq) VALUES (?, ?)",
        [(CYCLES + 1, seq) for seq in range(ROWS_PER_CYCLE)],
    )
# Snapshot with the db, WAL, and shm all live and DAX-mapped, then keep
# using and finally close them in the restored VM.
subprocess.run(["lnxctl", "snapshot-exit"], check=True)
with conn:
    conn.executemany(
        "INSERT INTO t (cycle, seq) VALUES (?, ?)",
        [(CYCLES + 2, seq) for seq in range(ROWS_PER_CYCLE)],
    )
busy, _, _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
assert busy == 0
conn.close()
assert not os.path.exists(DB + "-wal")
assert not os.path.exists(DB + "-shm")

conn = open_db()
(count,) = conn.execute("SELECT COUNT(*) FROM t").fetchone()
conn.close()
assert count == (CYCLES + 3) * ROWS_PER_CYCLE, count
print("wal-snapshot-ok", flush=True)
`,
    });
    assertContains(result.stdout, "wal-snapshot-ok", "WAL survived snapshot-exit and closed cleanly");
  });

  await testStep("guest can unlink and recreate a previously mapped WAL db", async () => {
    const result = await ctx.vm.cli(["python3", "-"], {
      cwd,
      timeoutMs: 180_000,
      stdin:
        pythonPrelude +
        String.raw`
os.unlink(DB)
conn = open_db()
conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, seq INTEGER)")
with conn:
    conn.executemany("INSERT INTO t (seq) VALUES (?)", [(seq,) for seq in range(ROWS_PER_CYCLE)])
busy, _, _ = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
assert busy == 0
conn.close()
print("wal-recreate-ok", flush=True)
`,
    });
    assertContains(result.stdout, "wal-recreate-ok", "unlink and recreate succeeded");
  });

  await testStep("host sees a consistent database after guest close", async () => {
    // Not readonly: a WAL-mode db cannot be opened by a readonly connection
    // unless its -shm still exists (SQLITE_CANTOPEN), and the guest's clean
    // close correctly removed it.
    const db = new Database(join(cwd, dbName));
    try {
      const row = db.query("SELECT COUNT(*) AS count FROM t").get() as { count: number };
      assertEq(row.count, rowsPerCycle, "host read recreated db row count");
    } finally {
      db.close();
    }
  });
} finally {
  await cleanupContext(ctx);
  await cleanupDirs();
}
