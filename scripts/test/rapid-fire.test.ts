import { test } from "bun:test";
import { runScript } from "./run-script";

test("rapid-fire", async () => runScript("rapid-fire"), 360_000);
