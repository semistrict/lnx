import { test } from "bun:test";
import { runScript } from "./run-script";

test("snapshot-compat", async () => runScript("snapshot-compat"), 300_000);
