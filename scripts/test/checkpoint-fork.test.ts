import { test } from "bun:test";
import { runScript } from "./run-script";

test("checkpoint-fork", async () => runScript("checkpoint-fork"), 300_000);
