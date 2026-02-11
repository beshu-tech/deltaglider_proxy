# DeltaGlider Storage Savings Report

## Test Configuration
- **Date**: Wed Dec 17 14:43:43 CET 2025
- **Artifacts**: Elasticsearch JAR files (40 versions)
- **Versions**: 7.0.0 7.1.0 7.2.0 7.3.0 7.4.0 7.5.0 7.6.0 7.7.0 7.8.0 7.9.0 7.10.0 7.11.0 7.12.0 7.13.0 7.14.0 7.15.0 7.16.0 7.17.0 7.17.10 7.17.20 7.17.25 8.0.0 8.1.0 8.2.0 8.3.0 8.4.0 8.5.0 8.6.0 8.7.0 8.8.0 8.9.0 8.10.0 8.11.0 8.12.0 8.13.0 8.14.0 8.15.0 8.16.0 8.17.0 8.18.0

## Results

### Baseline (Direct MinIO)
```
=== Baseline Storage ===
543MiB	40 objects	artifacts
[2025-12-17 14:42:12 CET]  10MiB STANDARD elasticsearch-7.0.0.jar
[2025-12-17 14:42:12 CET]  10MiB STANDARD elasticsearch-7.1.0.jar
[2025-12-17 14:42:13 CET]  12MiB STANDARD elasticsearch-7.10.0.jar
[2025-12-17 14:42:13 CET]  13MiB STANDARD elasticsearch-7.11.0.jar
[2025-12-17 14:42:13 CET]  13MiB STANDARD elasticsearch-7.12.0.jar
[2025-12-17 14:42:13 CET]  13MiB STANDARD elasticsearch-7.13.0.jar
[2025-12-17 14:42:14 CET]  13MiB STANDARD elasticsearch-7.14.0.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.15.0.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.16.0.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.17.0.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.17.10.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.17.20.jar
[2025-12-17 14:42:14 CET]  14MiB STANDARD elasticsearch-7.17.25.jar
[2025-12-17 14:42:12 CET]  11MiB STANDARD elasticsearch-7.2.0.jar
[2025-12-17 14:42:12 CET]  11MiB STANDARD elasticsearch-7.3.0.jar
[2025-12-17 14:42:12 CET]  11MiB STANDARD elasticsearch-7.4.0.jar
[2025-12-17 14:42:12 CET]  11MiB STANDARD elasticsearch-7.5.0.jar
[2025-12-17 14:42:13 CET]  11MiB STANDARD elasticsearch-7.6.0.jar
[2025-12-17 14:42:13 CET]  12MiB STANDARD elasticsearch-7.7.0.jar
[2025-12-17 14:42:13 CET]  12MiB STANDARD elasticsearch-7.8.0.jar
[2025-12-17 14:42:13 CET]  12MiB STANDARD elasticsearch-7.9.0.jar
[2025-12-17 14:42:14 CET]  13MiB STANDARD elasticsearch-8.0.0.jar
[2025-12-17 14:42:15 CET]  13MiB STANDARD elasticsearch-8.1.0.jar
[2025-12-17 14:42:16 CET]  15MiB STANDARD elasticsearch-8.10.0.jar
[2025-12-17 14:42:16 CET]  15MiB STANDARD elasticsearch-8.11.0.jar
[2025-12-17 14:42:16 CET]  16MiB STANDARD elasticsearch-8.12.0.jar
[2025-12-17 14:42:16 CET]  16MiB STANDARD elasticsearch-8.13.0.jar
[2025-12-17 14:42:16 CET]  16MiB STANDARD elasticsearch-8.14.0.jar
[2025-12-17 14:42:16 CET]  16MiB STANDARD elasticsearch-8.15.0.jar
[2025-12-17 14:42:16 CET]  17MiB STANDARD elasticsearch-8.16.0.jar
[2025-12-17 14:42:17 CET]  17MiB STANDARD elasticsearch-8.17.0.jar
[2025-12-17 14:42:17 CET]  17MiB STANDARD elasticsearch-8.18.0.jar
[2025-12-17 14:42:15 CET]  14MiB STANDARD elasticsearch-8.2.0.jar
[2025-12-17 14:42:15 CET]  14MiB STANDARD elasticsearch-8.3.0.jar
[2025-12-17 14:42:15 CET]  14MiB STANDARD elasticsearch-8.4.0.jar
[2025-12-17 14:42:15 CET]  14MiB STANDARD elasticsearch-8.5.0.jar
[2025-12-17 14:42:15 CET]  14MiB STANDARD elasticsearch-8.6.0.jar
[2025-12-17 14:42:15 CET]  15MiB STANDARD elasticsearch-8.7.0.jar
[2025-12-17 14:42:15 CET]  15MiB STANDARD elasticsearch-8.8.0.jar
[2025-12-17 14:42:15 CET]  15MiB STANDARD elasticsearch-8.9.0.jar
```

### DeltaGlider (Delta Compression)
```
=== DeltaGlider Storage ===
470MiB	41 objects	deltaglider-data
[2025-12-17 14:42:19 CET]    50B STANDARD elasticsearch-7.0.0.jar.delta
[2025-12-17 14:42:19 CET] 491KiB STANDARD elasticsearch-7.1.0.jar.delta
[2025-12-17 14:42:32 CET]  11MiB STANDARD elasticsearch-7.10.0.jar.delta
[2025-12-17 14:42:35 CET]  12MiB STANDARD elasticsearch-7.11.0.jar.delta
[2025-12-17 14:42:36 CET]  12MiB STANDARD elasticsearch-7.12.0.jar.delta
[2025-12-17 14:42:39 CET]  12MiB STANDARD elasticsearch-7.13.0.jar.delta
[2025-12-17 14:42:41 CET]  12MiB STANDARD elasticsearch-7.14.0.jar.delta
[2025-12-17 14:42:43 CET]  12MiB STANDARD elasticsearch-7.15.0.jar.delta
[2025-12-17 14:42:45 CET]  12MiB STANDARD elasticsearch-7.16.0.jar.delta
[2025-12-17 14:42:47 CET]  12MiB STANDARD elasticsearch-7.17.0.jar.delta
[2025-12-17 14:42:49 CET]  13MiB STANDARD elasticsearch-7.17.10.jar.delta
[2025-12-17 14:42:52 CET]  13MiB STANDARD elasticsearch-7.17.20.jar.delta
[2025-12-17 14:42:54 CET]  13MiB STANDARD elasticsearch-7.17.25.jar.delta
[2025-12-17 14:42:20 CET] 3.0MiB STANDARD elasticsearch-7.2.0.jar.delta
[2025-12-17 14:42:21 CET] 4.6MiB STANDARD elasticsearch-7.3.0.jar.delta
[2025-12-17 14:42:22 CET] 6.2MiB STANDARD elasticsearch-7.4.0.jar.delta
[2025-12-17 14:42:23 CET] 6.5MiB STANDARD elasticsearch-7.5.0.jar.delta
[2025-12-17 14:42:25 CET]  10MiB STANDARD elasticsearch-7.6.0.jar.delta
[2025-12-17 14:42:27 CET]  10MiB STANDARD elasticsearch-7.7.0.jar.delta
[2025-12-17 14:42:28 CET]  11MiB STANDARD elasticsearch-7.8.0.jar.delta
[2025-12-17 14:42:30 CET]  11MiB STANDARD elasticsearch-7.9.0.jar.delta
[2025-12-17 14:42:56 CET]  12MiB STANDARD elasticsearch-8.0.0.jar.delta
[2025-12-17 14:42:58 CET]  12MiB STANDARD elasticsearch-8.1.0.jar.delta
[2025-12-17 14:43:19 CET]  14MiB STANDARD elasticsearch-8.10.0.jar.delta
[2025-12-17 14:43:22 CET]  14MiB STANDARD elasticsearch-8.11.0.jar.delta
[2025-12-17 14:43:24 CET]  14MiB STANDARD elasticsearch-8.12.0.jar.delta
[2025-12-17 14:43:27 CET]  14MiB STANDARD elasticsearch-8.13.0.jar.delta
[2025-12-17 14:43:30 CET]  15MiB STANDARD elasticsearch-8.14.0.jar.delta
[2025-12-17 14:43:33 CET]  15MiB STANDARD elasticsearch-8.15.0.jar.delta
[2025-12-17 14:43:35 CET]  15MiB STANDARD elasticsearch-8.16.0.jar.delta
[2025-12-17 14:43:38 CET]  16MiB STANDARD elasticsearch-8.17.0.jar.delta
[2025-12-17 14:43:41 CET]  16MiB STANDARD elasticsearch-8.18.0.jar.delta
[2025-12-17 14:43:00 CET]  12MiB STANDARD elasticsearch-8.2.0.jar.delta
[2025-12-17 14:43:03 CET]  12MiB STANDARD elasticsearch-8.3.0.jar.delta
[2025-12-17 14:43:05 CET]  13MiB STANDARD elasticsearch-8.4.0.jar.delta
[2025-12-17 14:43:07 CET]  13MiB STANDARD elasticsearch-8.5.0.jar.delta
[2025-12-17 14:43:10 CET]  13MiB STANDARD elasticsearch-8.6.0.jar.delta
[2025-12-17 14:43:12 CET]  13MiB STANDARD elasticsearch-8.7.0.jar.delta
[2025-12-17 14:43:14 CET]  14MiB STANDARD elasticsearch-8.8.0.jar.delta
[2025-12-17 14:43:17 CET]  14MiB STANDARD elasticsearch-8.9.0.jar.delta
[2025-12-17 14:42:19 CET]  10MiB STANDARD reference.bin
```

## Summary

| Metric | Baseline | DeltaGlider |
|--------|----------|-------------|
| Storage | 543MiB | 470MiB |
| Savings | - | 20.0% |

## How It Works

DeltaGlider Proxy applies **xdelta3 delta compression** to similar files:

1. First file uploaded → stored as-is (base version)
2. Subsequent similar files → only the delta (difference) is stored
3. On retrieval → original file is reconstructed transparently

This is particularly effective for:
- Sequential software versions (like Elasticsearch 7.17.0 → 7.17.14)
- Similar artifacts with shared code
- Backup systems with incremental changes

## Architecture

```
Client → DeltaGlider Proxy → MinIO
              ↓
        xdelta3 compression
        (stores deltas only)
```
