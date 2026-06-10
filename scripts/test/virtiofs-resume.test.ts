import { test } from "bun:test";
import { runScript } from "./run-script";

test("virtiofs-resume", async () => runScript("virtiofs-resume"), 300_000);
