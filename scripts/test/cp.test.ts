import { test } from "bun:test";
import { runScript } from "./run-script";

test("cp", async () => runScript("cp"), 240_000);
