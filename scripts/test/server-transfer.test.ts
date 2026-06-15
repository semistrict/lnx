import { test } from "bun:test";
import { runScript } from "./run-script";

test("server-transfer", async () => runScript("server-transfer"), 120_000);
