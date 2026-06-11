import { test } from "bun:test";
import { runScript } from "./run-script";

test("snapshot-chaos", async () => runScript("snapshot-chaos"), 1_800_000);
