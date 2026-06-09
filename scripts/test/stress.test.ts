import { test } from "bun:test";
import { runScript } from "./run-script";

test("stress", async () => runScript("stress"), 300_000);
