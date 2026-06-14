import { test } from "bun:test";
import { runScript } from "./run-script";

test("linux-macos-snapshot", async () => runScript("linux-macos-snapshot"), 360_000);
