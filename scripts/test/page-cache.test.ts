import { test } from "bun:test";
import { runScript } from "./run-script";

test("page-cache", async () => runScript("page-cache"), 300_000);
