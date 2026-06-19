import { test } from "bun:test";
import { runScript } from "./run-script";

test("balloon", async () => runScript("balloon"), 300_000);
