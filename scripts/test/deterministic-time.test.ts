import { test } from "bun:test";
import { runScript } from "./run-script";

test("deterministic-time", async () => runScript("deterministic-time"), 360_000);
