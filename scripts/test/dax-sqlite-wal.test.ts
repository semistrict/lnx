import { test } from "bun:test";
import { runScript } from "./run-script";

test("dax-sqlite-wal", async () => runScript("dax-sqlite-wal"), 900_000);
