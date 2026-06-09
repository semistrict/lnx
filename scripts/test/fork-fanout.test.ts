import { test } from "bun:test";
import { runScript } from "./run-script";

test("fork-fanout", async () => runScript("fork-fanout"), 300_000);
