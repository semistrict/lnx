import { test } from "bun:test";
import { runScript } from "./run-script";

test("stock-ubuntu", async () => runScript("stock-ubuntu"), 900_000);
