import { test } from "bun:test";
import { runScript } from "./run-script";

test("system", async () => runScript("system"), 240_000);
