import { test } from "bun:test";
import { runScript } from "./run-script";

test("pty-resume", async () => runScript("pty-resume"), 300_000);
