import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.time.Instant;

public final class MinecondaSmokeMain {
    private MinecondaSmokeMain() {}

    public static void main(String[] args) throws Exception {
        String mode = System.getProperty("mineconda.mode", "unknown");
        String role = mode.contains("server") ? "server" : "client";
        String javaHome = System.getProperty("java.home", "");
        String workDir = System.getProperty("user.dir", "");
        String gameDir = System.getProperty("minecraft.gamedir", "");

        System.out.println("MINECONDA_SMOKE_START role=" + role + " mode=" + mode);
        System.out.println("MINECONDA_SMOKE_JAVA_HOME=" + javaHome);
        System.out.println("MINECONDA_SMOKE_WORKDIR=" + workDir);
        if (!gameDir.isEmpty()) {
            System.out.println("MINECONDA_SMOKE_GAMEDIR=" + gameDir);
        }

        for (int i = 0; i < args.length; i += 1) {
            System.out.println("MINECONDA_SMOKE_ARG[" + i + "]=" + args[i]);
        }
        System.out.flush();

        Path marker = Paths.get(workDir).resolve("mineconda-smoke-" + role + ".txt");
        writeMarker(marker, role, mode, args);
        System.out.println("MINECONDA_SMOKE_MARKER=" + marker.toAbsolutePath());
        System.out.flush();

        long serverSleepMs = Long.getLong("mineconda.smoke.server_sleep_ms", 0L);
        if (role.equals("server") && serverSleepMs > 0) {
            Thread.sleep(serverSleepMs);
        }

        System.out.println("MINECONDA_SMOKE_DONE role=" + role);
        System.out.flush();
    }

    private static void writeMarker(Path marker, String role, String mode, String[] args)
        throws IOException {
        Files.createDirectories(marker.getParent());
        StringBuilder payload = new StringBuilder();
        payload.append("role=").append(role).append('\n');
        payload.append("mode=").append(mode).append('\n');
        payload.append("timestamp=").append(Instant.now()).append('\n');
        for (int i = 0; i < args.length; i += 1) {
            payload.append("arg[").append(i).append("]=").append(args[i]).append('\n');
        }
        Files.writeString(marker, payload.toString(), StandardCharsets.UTF_8);
    }
}
