# Jet Server API Specification

This document provides a comprehensive overview of the Jet Server API. Jet is a high-performance, sandboxed code execution engine.

## Base URL

The server typically runs on `http://localhost:4000` (configurable).

## Rate Limiting

- **Strict Routes** (`POST /jobs`): 1 request per second per IP (Burst: 3).
- **General Routes** (all other endpoints): 5 requests per second per IP (Burst: 10).

IP extraction respects `X-Forwarded-For` and `X-Real-IP` headers from reverse proxies.

---

## Endpoints

### 1. Health Check

`GET /health`

Returns the health status of the server.

- **Response Type**: `text/plain`
- **Success (200 OK)**:
  ```text
  ok
  ```

### 2. Server Statistics

`GET /stats`

Returns runtime statistics, uptime, and configuration limits.

- **Response Type**: `application/json`
- **Success (200 OK)**:
  ```json
  {
    "uptime_seconds": 3600,
    "jobs_submitted": 150,
    "jobs_completed": 145,
    "jobs_failed": 5,
    "jobs_in_flight": 2,
    "compile_in_flight": 1,
    "execute_in_flight": 1,
    "max_queue_depth": 100,
    "installed_runtimes": 12,
    "supported_languages": ["python", "javascript", "cpp", "rust"],
    "worker_concurrency": 8,
    "compile_concurrency": 2,
    "execute_concurrency": 6,
    "max_queue_wait_secs": 30,
    "host_arch": "x86_64"
  }
  ```

### 3. List Runtimes

`GET /runtimes`

Lists all installed runtimes grouped by language.

- **Response Type**: `application/json`
- **Success (200 OK)**:
  ```json
  {
    "total": 3,
    "languages": {
      "python": [
        {
          "version": "3.12.0",
          "aliases": ["py", "python3"],
          "architectures": ["x86_64"],
          "compiled": false
        }
      ],
      "cpp": [
        {
          "version": "gcc-13",
          "aliases": ["cpp", "g++"],
          "architectures": ["x86_64"],
          "compiled": true
        }
      ]
    }
  }
  ```

### 4. List Runtimes for Language

`GET /runtimes/{language}`

Lists runtimes for a specific language.

- **Path Parameters**:
  - `language`: The language name (e.g., `python`, `cpp`).
- **Response Type**: `application/json`
- **Success (200 OK)**:
  ```json
  {
    "total": 1,
    "languages": {
      "python": [
        {
          "version": "3.12.0",
          "aliases": ["py", "python3"],
          "architectures": ["x86_64"],
          "compiled": false
        }
      ]
    }
  }
  ```
- **Error (404 Not Found)**:
  ```json
  "no installed runtimes for language: unknown-lang"
  ```

### 5. Submit Execution Job

`POST /jobs` (Strict Rate Limit)

Submits code for execution in a sandboxed environment. Supports two modes:

- **Single-run mode**: Runs the code once with optional `stdin`.
- **Testcase mode**: Runs the code once per testcase with per-case input and optional expected output comparison.

> **Note**: `stdin` and `testcases` are mutually exclusive at the execution level. When `testcases` is provided, each testcase's `input` is used as stdin for that run; the top-level `stdin` field is ignored.

#### 5a. Single-Run Request

- **Request Body (JSON)**:
  ```json
  {
    "language": "python",
    "version": "3.12.0",
    "files": [
      {
        "name": "main.py",
        "content": "print('Hello, World!')"
      }
    ],
    "stdin": "optional input",
    "args": ["--flag", "value"],
    "run_timeout": 3000,
    "run_memory_limit": 268435456
  }
  ```

#### 5b. Testcase Batch Request

Submit code with multiple testcases. Each testcase runs the program independently with its own stdin input. If `expected_output` is provided, the server automatically compares the program's trimmed stdout against it and sets the `passed` flag.

- **Request Body (JSON)**:
  ```json
  {
    "language": "python",
    "version": "3.12.0",
    "files": [
      {
        "name": "solution.py",
        "content": "n = int(input())\nprint(n * 2)"
      }
    ],
    "testcases": [
      {
        "id": "tc-1",
        "input": "5",
        "expected_output": "10"
      },
      {
        "id": "tc-2",
        "input": "0",
        "expected_output": "0"
      },
      {
        "id": "tc-3",
        "input": "42"
      }
    ],
    "run_timeout": 2000,
    "run_memory_limit": 134217728
  }
  ```

#### 5c. Compiled Language Request (with all options)

For compiled languages (e.g., C++, Java, Rust), the server performs a compile step before execution. Both compile and run stages have independent limit overrides.

- **Request Body (JSON)**:
  ```json
  {
    "language": "cpp",
    "version": "gcc-13",
    "files": [
      {
        "name": "main.cpp",
        "content": "#include <iostream>\nint main() { int n; std::cin >> n; std::cout << n * 2; }"
      }
    ],
    "testcases": [
      {
        "id": "case-1",
        "input": "21",
        "expected_output": "42"
      }
    ],
    "compile_timeout": 10000,
    "compile_memory_limit": 1073741824,
    "compile_output_limit": 1048576,
    "run_timeout": 2000,
    "run_memory_limit": 268435456,
    "run_output_limit": 1048576
  }
  ```

#### Response

- **Response Type**: `application/json`
- **Success (202 Accepted)**:
  ```json
  {
    "job_id": "550e8400-e29b-41d4-a716-446655440000",
    "status": "queued",
    "resolved_version": "3.12.0"
  }
  ```
- **Errors**:
  - `400 Bad Request`: Validation failure.
    - `"at least one file is required"`
    - `"too many files: 11 (max: 10)"`
    - `"too many testcases: 1001 (max: 1000)"`
    - `"testcase 3 input too large"` (max per-testcase input: 512 KB)
    - `"version is required"`
    - `"runtime not installed or unsupported: python:9.9.9"`
  - `413 Payload Too Large`: Total file size > 5 MB (`"total file size too large: 6000000 bytes (max: 5242880)"`).
  - `429 Too Many Requests`: Capacity reached (`"server is overloaded: 100 jobs in flight (max: 100)"`).

### 6. Get Job Result

`GET /jobs/{id}`

Retrieves the current status and result of a job. Job results are kept in Redis for 1 hour (TTL: 3600s).

- **Path Parameters**:
  - `id`: Unique Job ID returned from `POST /jobs`.
- **Response Type**: `application/json`

#### 6a. Queued / In-Progress

  ```json
  {
    "job_id": "550e8400-e29b-41d4-a716-446655440000",
    "status": "queued",
    "language": "python",
    "version": "3.12.0",
    "result": null,
    "error": null
  }
  ```

#### 6b. Completed — Single Run (interpreted language)

  ```json
  {
    "job_id": "550e8400-e29b-41d4-a716-446655440000",
    "status": "completed",
    "language": "python",
    "version": "3.12.0",
    "result": {
      "language": "python",
      "version": "3.12.0",
      "run": {
        "status": "SUCCESS",
        "stdout": "Hello, World!\n",
        "stderr": "",
        "exit_code": 0,
        "signal": null,
        "memory_usage": 15423488,
        "cpu_time": 15000,
        "execution_time": 45
      },
      "compile": null,
      "testcases": null
    },
    "error": null,
    "queue_wait_ms": 12
  }
  ```

#### 6c. Completed — Single Run (compiled language)

  ```json
  {
    "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "status": "completed",
    "language": "cpp",
    "version": "gcc-13",
    "result": {
      "language": "cpp",
      "version": "gcc-13",
      "compile": {
        "status": "SUCCESS",
        "stdout": "",
        "stderr": "",
        "exit_code": 0,
        "signal": null,
        "memory_usage": 52428800,
        "cpu_time": 850000,
        "execution_time": 1200
      },
      "run": {
        "status": "SUCCESS",
        "stdout": "42",
        "stderr": "",
        "exit_code": 0,
        "signal": null,
        "memory_usage": 3145728,
        "cpu_time": 1200,
        "execution_time": 5
      },
      "testcases": null
    },
    "error": null,
    "queue_wait_ms": 8
  }
  ```

#### 6d. Completed — Testcase Batch

When testcases are submitted, `run` is `null` and `testcases` contains a result for each submitted testcase.

  ```json
  {
    "job_id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
    "status": "completed",
    "language": "python",
    "version": "3.12.0",
    "result": {
      "language": "python",
      "version": "3.12.0",
      "run": null,
      "compile": null,
      "testcases": [
        {
          "id": "tc-1",
          "passed": true,
          "actual_output": "10\n",
          "run_details": {
            "status": "SUCCESS",
            "stdout": "10\n",
            "stderr": "",
            "exit_code": 0,
            "signal": null,
            "memory_usage": 14680064,
            "cpu_time": 12000,
            "execution_time": 38
          }
        },
        {
          "id": "tc-2",
          "passed": true,
          "actual_output": "0\n",
          "run_details": {
            "status": "SUCCESS",
            "stdout": "0\n",
            "stderr": "",
            "exit_code": 0,
            "signal": null,
            "memory_usage": 14680064,
            "cpu_time": 11500,
            "execution_time": 35
          }
        },
        {
          "id": "tc-3",
          "passed": true,
          "actual_output": "84\n",
          "run_details": {
            "status": "SUCCESS",
            "stdout": "84\n",
            "stderr": "",
            "exit_code": 0,
            "signal": null,
            "memory_usage": 14680064,
            "cpu_time": 11800,
            "execution_time": 36
          }
        }
      ]
    },
    "error": null,
    "queue_wait_ms": 5
  }
  ```

> **Note**: When no `expected_output` is provided for a testcase (like `tc-3` above), `passed` is `true` as long as execution succeeds.

#### 6e. Compilation Error

If the compile step fails, execution is skipped entirely. The `compile` field contains the error details.

  ```json
  {
    "job_id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
    "status": "completed",
    "language": "cpp",
    "version": "gcc-13",
    "result": {
      "language": "cpp",
      "version": "gcc-13",
      "compile": {
        "status": "COMPILATION_ERROR",
        "stdout": "",
        "stderr": "main.cpp:3:1: error: expected ';' after expression\n",
        "exit_code": 1,
        "signal": null,
        "memory_usage": 41943040,
        "cpu_time": 320000,
        "execution_time": 500
      },
      "run": null,
      "testcases": null
    },
    "error": null,
    "queue_wait_ms": 3
  }
  ```

#### 6f. Failed Job

  ```json
  {
    "job_id": "d4e5f6a7-b8c9-0123-defa-234567890123",
    "status": "failed",
    "language": "python",
    "version": "3.12.0",
    "result": null,
    "error": "sandbox setup failed: permission denied",
    "queue_wait_ms": 15
  }
  ```

#### 6g. Not Found

- **Error (404 Not Found)**:
  ```json
  "job not found: 550e8400-e29b-41d4-a716-446655440000"
  ```

---

## Data Models

### JobRequest

| Field | Type | Required | Description |
| :--- | :--- | :---: | :--- |
| `language` | String | **Yes** | Target language (e.g., `"python"`, `"cpp"`, `"java"`). |
| `version` | String | **Yes** | Desired runtime version or alias (e.g., `"3.12.0"`, `"gcc-13"`). Resolved to an installed version by the server. |
| `files` | Array\<FileRequest\> | **Yes** | Source files to execute (min: 1, max: 10). The first file is treated as the primary/entry file. Total size across all files must not exceed 5 MB. |
| `job_id` | String | No | Client-supplied job ID. If omitted, a UUID v4 is generated by the server. |
| `testcases` | Array\<Testcase\> | No | Test cases for batch execution (max: 1000). When provided, the program runs once per testcase. See [Testcase](#testcase). |
| `stdin` | String | No | Standard input fed to the program (single-run mode only; ignored when `testcases` is provided). |
| `args` | Array\<String\> | No | Command-line arguments passed to the program. |
| `run_timeout` | Integer | No | Execution time limit in milliseconds. Server default: 3000 ms. |
| `run_memory_limit` | Integer | No | Execution memory limit in bytes. Server default: 512 MB. |
| `run_output_limit` | Integer | No | Max stdout+stderr size in bytes for the run stage. Server default: 1 MB. |
| `compile_timeout` | Integer | No | Compilation time limit in milliseconds (compiled languages only). Server default: 3000 ms. |
| `compile_memory_limit` | Integer | No | Compilation memory limit in bytes (compiled languages only). Server default: 1 GB (2 GB for Java). |
| `compile_output_limit` | Integer | No | Max stdout+stderr size in bytes for the compile stage. Server default: 1 MB. |

### FileRequest

| Field | Type | Required | Description |
| :--- | :--- | :---: | :--- |
| `name` | String | No | File name (e.g., `"main.py"`, `"Solution.java"`). Defaults to `"main"` if omitted. |
| `content` | String | **Yes** | File content (source code). |
| `encoding` | String | No | Content encoding. Reserved for future use (e.g., `"base64"`). |

### Testcase

Each testcase runs the program in an independent sandbox with its own resource limits.

| Field | Type | Required | Description |
| :--- | :--- | :---: | :--- |
| `id` | String | **Yes** | Unique identifier for the testcase (e.g., `"tc-1"`, `"case-alpha"`). Returned as-is in results. |
| `input` | String | **Yes** | Standard input fed to the program for this testcase. Max size: 512 KB (1/10 of the 5 MB file limit). |
| `expected_output` | String | No | Expected stdout output. When provided, the server trims both actual and expected output and compares them to set the `passed` flag. When omitted, `passed` is `true` if execution succeeds. |

### SubmitJobResponse

| Field | Type | Description |
| :--- | :--- | :--- |
| `job_id` | String | Unique job identifier (server-generated UUID v4 or client-supplied). |
| `status` | String | Always `"queued"` on successful submission. |
| `resolved_version` | String | The exact installed runtime version the job will use. |

### JobStateRecord

Returned by `GET /jobs/{id}`.

| Field | Type | Description |
| :--- | :--- | :--- |
| `job_id` | String | Unique job identifier. |
| `status` | String | One of: `"queued"`, `"running"`, `"completed"`, `"failed"`. |
| `language` | String | Target language. |
| `version` | String | Resolved runtime version. |
| `result` | JobResult \| null | Execution result (present when `status` is `"completed"`). |
| `error` | String \| null | Error message (present when `status` is `"failed"`). |
| `queue_wait_ms` | Integer \| null | Milliseconds the job waited in the queue before a worker picked it up. Omitted while still queued. |

### JobResult

| Field | Type | Description |
| :--- | :--- | :--- |
| `language` | String | Language of the executed code. |
| `version` | String | Runtime version used. |
| `compile` | StageResult \| null | Compilation result (only for compiled languages; `null` for interpreted). |
| `run` | StageResult \| null | Execution result (single-run mode). `null` when testcases are used. |
| `testcases` | Array\<TestcaseResult\> \| null | Per-testcase results (testcase mode). `null` when running in single-run mode. |

### StageResult

Represents the outcome of a single execution stage (compile or run).

| Field | Type | Description |
| :--- | :--- | :--- |
| `status` | StageStatus | Outcome status of this stage. |
| `stdout` | String | Captured standard output. |
| `stderr` | String | Captured standard error. |
| `exit_code` | Integer \| null | Process exit code (`null` if killed by signal). |
| `signal` | String \| null | Signal name if the process was killed (e.g., `"SIGKILL"`, `"SIGSEGV"`). |
| `memory_usage` | Integer \| null | Peak memory usage in bytes. |
| `cpu_time` | Integer \| null | CPU time consumed in microseconds. |
| `execution_time` | Integer \| null | Wall-clock execution time in milliseconds. |

### TestcaseResult

| Field | Type | Description |
| :--- | :--- | :--- |
| `id` | String | Testcase identifier (matches the `id` from the request). |
| `passed` | Boolean | `true` if execution succeeded and output matches `expected_output` (or no `expected_output` was provided). |
| `actual_output` | String | The program's stdout for this testcase. |
| `run_details` | StageResult | Full execution details for this testcase run. |

### StageStatus (Enum)

| Value | Description |
| :--- | :--- |
| `PENDING` | Stage has not started yet. |
| `RUNNING` | Stage is currently executing. |
| `SUCCESS` | Stage completed successfully (exit code 0). |
| `RUNTIME_ERROR` | Program exited with a non-zero exit code or was killed by a signal. |
| `COMPILATION_ERROR` | Compilation failed (non-zero exit code from compiler). |
| `TIME_LIMIT_EXCEEDED` | Execution exceeded the configured timeout. |
| `MEMORY_LIMIT_EXCEEDED` | Execution exceeded the configured memory limit. |
| `OUTPUT_LIMIT_EXCEEDED` | Program produced more output than the configured output limit. |

---

## Execution Behavior

### Single-Run vs. Testcase Mode

| Aspect | Single-Run Mode | Testcase Mode |
| :--- | :--- | :--- |
| Input | `stdin` field | Each testcase's `input` field |
| Runs | 1 | 1 per testcase |
| Result location | `result.run` | `result.testcases[]` |
| Output comparison | Not performed | Automatic when `expected_output` is provided |
| Sandbox isolation | One sandbox | Independent sandbox per testcase |

### Compilation

For compiled languages (where the runtime manifest defines a compile step):

1. **Compile stage** runs first with its own limits (`compile_timeout`, `compile_memory_limit`, `compile_output_limit`).
2. If compilation **fails**, the job result is returned immediately with the compile error — no run/testcase execution occurs.
3. If compilation **succeeds**, the compiled output is used for all subsequent runs (single or per-testcase).

### Default Limits

| Limit | Default Value |
| :--- | :--- |
| `run_timeout` | 3000 ms |
| `run_memory_limit` | 512 MB |
| `run_output_limit` | 1 MB |
| `compile_timeout` | 3000 ms |
| `compile_memory_limit` | 1 GB (2 GB for Java) |
| `compile_output_limit` | 1 MB |
| PID limit | 256 |
| Open file limit | 2048 |

### Validation Limits

| Constraint | Limit |
| :--- | :--- |
| Max files per request | 10 |
| Max total file size | 5 MB (5,242,880 bytes) |
| Max testcases per request | 1000 |
| Max testcase input size | 512 KB (1/10 of max total file size) |

---

## Error Codes

| HTTP Code | Name | Typical Cause |
| :--- | :--- | :--- |
| `400` | Bad Request | Invalid JSON, missing required fields, too many files/testcases, testcase input too large, or unsupported runtime. |
| `404` | Not Found | Job ID not found or language has no installed runtimes. |
| `413` | Payload Too Large | Total file content exceeds 5 MB. |
| `429` | Too Many Requests | Rate limit exceeded or server queue is full. |
| `500` | Internal Server Error | Unexpected internal error (Redis connectivity, sandbox setup failure, etc.). |
