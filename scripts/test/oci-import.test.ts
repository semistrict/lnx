import { test } from "bun:test";
import { runScript } from "./run-script";

test("oci-import", async () => runScript("oci-import"), 1_200_000);
