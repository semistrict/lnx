import { test } from "bun:test";
import { runScript } from "./run-script";

test("virtiofs-policy", async () => runScript("virtiofs-policy"), 600_000);
