import { test } from "bun:test";
import { runScript } from "./run-script";

test("macos-linux-snapshot", async () => runScript("macos-linux-snapshot"), 300_000);
