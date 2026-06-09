import { test } from "bun:test";
import { runScript } from "./run-script";

test("broker-recovery", async () => runScript("broker-recovery"), 240_000);
