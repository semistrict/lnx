import { test } from "bun:test";
import { runScript } from "./run-script";

test("instance-config", async () => runScript("instance-config"), 600_000);
