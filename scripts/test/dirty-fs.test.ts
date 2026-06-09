import { test } from "bun:test";
import { runScript } from "./run-script";

test("dirty-fs", async () => runScript("dirty-fs"), 300_000);
