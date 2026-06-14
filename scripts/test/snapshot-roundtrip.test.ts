import { test } from "bun:test";
import { runScript } from "./run-script";

test("snapshot-roundtrip", async () => runScript("snapshot-roundtrip"), 2_400_000);
