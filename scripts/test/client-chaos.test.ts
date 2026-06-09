import { test } from "bun:test";
import { runScript } from "./run-script";

test("client-chaos", async () => runScript("client-chaos"), 240_000);
