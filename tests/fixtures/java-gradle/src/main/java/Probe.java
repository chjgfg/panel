public class Probe {
    public static void main(String[] args) throws Exception {
        String expected = System.getenv("PANEL_EXPECT_ARGUMENT");
        if (expected != null && (args.length != 1 || !args[0].equals(expected))) {
            throw new IllegalArgumentException("程序参数没有完整透传");
        }
        System.out.println("JAVA_LAUNCH_OK");
        if (System.getenv("PANEL_PROBE_SLEEP") != null) Thread.sleep(30000);
    }
}
