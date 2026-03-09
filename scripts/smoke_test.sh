#!/usr/bin/env bash
#
# Jet Server API Smoke Test Suite
# Tests all endpoints and various execution scenarios
#

# Configuration
BASE_URL="${BASE_URL:-http://localhost:4000}"
TIMEOUT=60
POLL_INTERVAL=0.5
RATE_LIMIT_DELAY=1.5  # Delay between job submissions to avoid rate limiting
TEST_COUNT=0
PASS_COUNT=0
FAIL_COUNT=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Utility functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $1"
    ((PASS_COUNT++))
}

log_error() {
    echo -e "${RED}[FAIL]${NC} $1"
    ((FAIL_COUNT++))
}

log_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

test_start() {
    ((TEST_COUNT++))
    echo ""
    log_info "Test $TEST_COUNT: $1"
}

# Submit job with rate-limit handling
submit_job() {
    local payload=$1
    local max_retries=5
    local retry=0
    
    while [ $retry -lt $max_retries ]; do
        response=$(curl -s -w "\n%{http_code}" -X POST "$BASE_URL/jobs" \
            -H "Content-Type: application/json" \
            -d "$payload")
        
        http_code=$(echo "$response" | tail -n1)
        body=$(echo "$response" | head -n-1)
        
        if [ "$http_code" = "202" ]; then
            echo "$body"
            sleep $RATE_LIMIT_DELAY  # Respect rate limits
            return 0
        elif [ "$http_code" = "429" ]; then
            retry=$((retry + 1))
            if [ $retry -lt $max_retries ]; then
                local wait_time=$((retry * 2))
                log_warning "Rate limited, waiting ${wait_time}s before retry $retry/$max_retries..."
                sleep $wait_time
            fi
        else
            echo "$body"
            return 1
        fi
    done
    
    echo "Rate limit exceeded after $max_retries retries"
    return 1
}

# Wait for job to complete and get result
wait_for_job() {
    local job_id=$1
    local timeout=$2
    local attempts=$((timeout * 2))  # Poll every 0.5 seconds
    local count=0
    
    while [ $count -lt $attempts ]; do
        response=$(curl -s "$BASE_URL/jobs/$job_id")
        status=$(echo "$response" | jq -r '.status')
        
        if [ "$status" = "completed" ] || [ "$status" = "failed" ]; then
            echo "$response"
            return 0
        fi
        
        sleep $POLL_INTERVAL
        count=$((count + 1))
    done
    
    log_error "Job $job_id timed out after ${timeout}s (last status: $status)"
    return 1
}

# Check if server is running
check_server() {
    if ! curl -s --connect-timeout 5 "$BASE_URL/health" > /dev/null 2>&1; then
        log_error "Cannot connect to server at $BASE_URL"
        echo "Please start the server first: cargo run --release --bin jet-server"
        exit 1
    fi
}

# ============================================================================
# TEST SUITE
# ============================================================================

echo "======================================================================"
echo "                   Jet Server API Smoke Test Suite"
echo "======================================================================"
echo "Server: $BASE_URL"
echo "======================================================================"

check_server

# ----------------------------------------------------------------------------
# 1. HEALTH & STATS ENDPOINTS
# ----------------------------------------------------------------------------

test_start "Health Check Endpoint"
response=$(curl -s "$BASE_URL/health")
if [ "$response" = "ok" ]; then
    log_success "Health check returned 'ok'"
else
    log_error "Health check failed. Expected 'ok', got: $response"
fi

test_start "Server Statistics Endpoint"
response=$(curl -s "$BASE_URL/stats")
if echo "$response" | jq -e '.uptime_seconds' > /dev/null 2>&1; then
    uptime=$(echo "$response" | jq -r '.uptime_seconds')
    worker_concurrency=$(echo "$response" | jq -r '.worker_concurrency')
    log_success "Stats endpoint returned valid data (uptime: ${uptime}s, workers: $worker_concurrency)"
else
    log_error "Stats endpoint returned invalid response"
fi

# ----------------------------------------------------------------------------
# 2. RUNTIME ENDPOINTS
# ----------------------------------------------------------------------------

test_start "List All Runtimes"
response=$(curl -s "$BASE_URL/runtimes")
if echo "$response" | jq -e '.total' > /dev/null 2>&1; then
    total=$(echo "$response" | jq -r '.total')
    languages=$(echo "$response" | jq -r '.languages | keys | join(", ")')
    log_success "Runtimes endpoint returned $total runtimes ($languages)"
else
    log_error "Runtimes endpoint returned invalid response"
fi

test_start "List Python Runtimes"
response=$(curl -s "$BASE_URL/runtimes/python")
if echo "$response" | jq -e '.total' > /dev/null 2>&1; then
    total=$(echo "$response" | jq -r '.total')
    if [ "$total" -gt 0 ]; then
        version=$(echo "$response" | jq -r '.languages.python[0].version')
        log_success "Found $total Python runtime(s) - using version $version for tests"
        PYTHON_VERSION=$version
    else
        log_warning "No Python runtimes installed"
        PYTHON_VERSION=""
    fi
else
    log_error "Python runtimes endpoint returned invalid response"
fi

test_start "List C Runtimes"
response=$(curl -s "$BASE_URL/runtimes/c")
if echo "$response" | jq -e '.total' > /dev/null 2>&1; then
    total=$(echo "$response" | jq -r '.total')
    if [ "$total" -gt 0 ]; then
        version=$(echo "$response" | jq -r '.languages.c[0].version')
        log_success "Found $total C runtime(s) - using version $version for tests"
        C_VERSION=$version
    else
        log_warning "No C runtimes installed"
        C_VERSION=""
    fi
else
    log_error "C runtimes endpoint returned invalid response"
fi

test_start "List Rust Runtimes"
response=$(curl -s "$BASE_URL/runtimes/rust")
if echo "$response" | jq -e '.total' > /dev/null 2>&1; then
    total=$(echo "$response" | jq -r '.total')
    if [ "$total" -gt 0 ]; then
        version=$(echo "$response" | jq -r '.languages.rust[0].version')
        log_success "Found $total Rust runtime(s) - using version $version for tests"
        RUST_VERSION=$version
    else
        log_warning "No Rust runtimes installed"
        RUST_VERSION=""
    fi
else
    log_error "Rust runtimes endpoint returned invalid response"
fi

test_start "Invalid Language Query"
response=$(curl -s -w "\n%{http_code}" "$BASE_URL/runtimes/nonexistent-language")
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | head -n-1)
if [ "$http_code" = "404" ]; then
    log_success "Correctly returned 404 for nonexistent language"
else
    log_error "Expected 404, got $http_code"
fi

# ----------------------------------------------------------------------------
# 3. BASIC EXECUTION TESTS
# ----------------------------------------------------------------------------

if [ -n "$PYTHON_VERSION" ]; then
    test_start "Python Hello World (Single Run)"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
            "files": [
                {
                    "name": "main.py",
                    "content": "print(\"Hello from Python!\")"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        status=$(echo "$result" | jq -r '.result.run.status')
        stdout=$(echo "$result" | jq -r '.result.run.stdout')
        
        if [ "$status" = "SUCCESS" ] && echo "$stdout" | grep -q "Hello from Python!"; then
            log_success "Python hello world executed successfully"
        else
            log_error "Python execution failed. Status: $status, Output: $stdout"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$C_VERSION" ]; then
    test_start "C Hello World (Compiled Language)"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "#include <stdio.h>\nint main() {\n    printf(\"Hello from C!\\n\");\n    return 0;\n}"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        compile_status=$(echo "$result" | jq -r '.result.compile.status')
        run_status=$(echo "$result" | jq -r '.result.run.status')
        stdout=$(echo "$result" | jq -r '.result.run.stdout')
        
        if [ "$compile_status" = "SUCCESS" ] && [ "$run_status" = "SUCCESS" ] && echo "$stdout" | grep -q "Hello from C!"; then
            log_success "C hello world compiled and executed successfully"
        else
            log_error "C execution failed. Compile: $compile_status, Run: $run_status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 4. IO OPERATIONS TESTS
# ----------------------------------------------------------------------------

if [ -n "$PYTHON_VERSION" ]; then
    test_start "Python with STDIN Input"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
            "files": [
                {
                    "name": "main.py",
                    "content": "name = input()\nprint(f\"Hello, {name}!\")"
                }
            ],
            "stdin": "World"
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        stdout=$(echo "$result" | jq -r '.result.run.stdout')
        
        if echo "$stdout" | grep -q "Hello, World!"; then
            log_success "STDIN input processed correctly"
        else
            log_error "STDIN test failed. Output: $stdout"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$C_VERSION" ]; then
    test_start "C with STDIN/STDOUT (Math Operations)"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "#include <stdio.h>\nint main() {\n    int a, b;\n    scanf(\"%d %d\", &a, &b);\n    printf(\"%d\\n\", a + b);\n    return 0;\n}"
                }
            ],
            "stdin": "5 10"
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        stdout=$(echo "$result" | jq -r '.result.run.stdout')
        
        if echo "$stdout" | grep -q "15"; then
            log_success "Math operations with I/O work correctly"
        else
            log_error "Math I/O test failed. Output: $stdout"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 5. TESTCASE-BASED EXECUTION
# ----------------------------------------------------------------------------

if [ -n "$PYTHON_VERSION" ]; then
    test_start "Testcase-Based Execution (Multiple Test Cases)"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
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
                    "input": "42",
                    "expected_output": "84"
                },
                {
                    "id": "tc-4",
                    "input": "-7",
                    "expected_output": "-14"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        testcases=$(echo "$result" | jq -r '.result.testcases')
        
        if [ "$testcases" != "null" ]; then
            passed_count=$(echo "$testcases" | jq '[.[] | select(.passed == true)] | length')
            total_count=$(echo "$testcases" | jq 'length')
            
            if [ "$passed_count" = "$total_count" ]; then
                log_success "All $total_count testcases passed"
            else
                log_error "Only $passed_count/$total_count testcases passed"
                echo "$testcases" | jq '.[] | select(.passed == false) | {id, actual_output, expected_output}'
            fi
        else
            log_error "No testcases in result"
        fi
    else
        log_error "Failed to submit job: $response"
    fi

    test_start "Testcase with Wrong Expected Output"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
            "files": [
                {
                    "name": "solution.py",
                    "content": "print(\"Hello\")"
                }
            ],
            "testcases": [
                {
                    "id": "should-fail",
                    "input": "",
                    "expected_output": "Goodbye"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        passed=$(echo "$result" | jq -r '.result.testcases[0].passed')
        
        if [ "$passed" = "false" ]; then
            log_success "Testcase correctly marked as failed due to wrong output"
        else
            log_error "Testcase should have failed but passed"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 6. ERROR SCENARIOS
# ----------------------------------------------------------------------------

if [ -n "$PYTHON_VERSION" ]; then
    test_start "Time Limit Exceeded (TLE)"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
            "files": [
                {
                    "name": "main.py",
                    "content": "while True:\n    pass"
                }
            ],
            "run_timeout": 1000
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        status=$(echo "$result" | jq -r '.result.run.status')
        
        if [ "$status" = "TIME_LIMIT_EXCEEDED" ]; then
            log_success "Time limit exceeded detected correctly"
        else
            log_error "Expected TIME_LIMIT_EXCEEDED, got: $status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$RUST_VERSION" ]; then
    test_start "Memory Limit Exceeded (MLE)"
    response=$(submit_job '{
            "language": "rust",
            "version": "'"$RUST_VERSION"'",
            "files": [
                {
                    "name": "main.rs",
                    "content": "fn main() {\n    let mut chunks: Vec<Vec<u8>> = Vec::new();\n    loop {\n        let chunk = vec![0u8; 10 * 1024 * 1024];\n        chunks.push(chunk);\n    }\n}"
                }
            ],
            "run_memory_limit": 67108864
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        compile_status=$(echo "$result" | jq -r '.result.compile.status')
        run_status=$(echo "$result" | jq -r '.result.run.status')
        
        if [ "$compile_status" = "SUCCESS" ] && [ "$run_status" = "MEMORY_LIMIT_EXCEEDED" ]; then
            log_success "Memory limit exceeded detected correctly"
        else
            log_error "Expected MEMORY_LIMIT_EXCEEDED, got compile: $compile_status, run: $run_status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$C_VERSION" ]; then
    test_start "Output Limit Exceeded (OLE)"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "#include <stdio.h>\nint main() {\n    while(1) {\n        printf(\"Flooding the output buffer with junk data...\\n\");\n    }\n    return 0;\n}"
                }
            ],
            "run_output_limit": 4096,
            "run_timeout": 2000
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        run_status=$(echo "$result" | jq -r '.result.run.status')
        
        if [ "$run_status" = "OUTPUT_LIMIT_EXCEEDED" ]; then
            log_success "Output limit exceeded detected correctly"
        else
            log_error "Expected OUTPUT_LIMIT_EXCEEDED, got: $run_status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$PYTHON_VERSION" ]; then
    test_start "Runtime Error (Division by Zero)"
    response=$(submit_job '{
            "language": "python",
            "version": "'"$PYTHON_VERSION"'",
            "files": [
                {
                    "name": "main.py",
                    "content": "x = 10 / 0"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        status=$(echo "$result" | jq -r '.result.run.status')
        stderr=$(echo "$result" | jq -r '.result.run.stderr')
        
        if [ "$status" = "RUNTIME_ERROR" ] && echo "$stderr" | grep -qi "division"; then
            log_success "Runtime error detected correctly"
        else
            log_error "Expected RUNTIME_ERROR with division error, got status: $status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 7. COMPILATION ERRORS
# ----------------------------------------------------------------------------

if [ -n "$RUST_VERSION" ]; then
    test_start "Compilation Error (Rust)"
    response=$(submit_job '{
            "language": "rust",
            "version": "'"$RUST_VERSION"'",
            "files": [
                {
                    "name": "main.rs",
                    "content": "use std::io;\n\nfn main() {\n  let name = String::new();\n  io::stdin::readline(name).unwrap();\n  println!(\"Hello, World! {}\", name);\n}"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        compile_status=$(echo "$result" | jq -r '.result.compile.status')
        run=$(echo "$result" | jq -r '.result.run')
        
        if [ "$compile_status" = "COMPILATION_ERROR" ] && [ "$run" = "null" ]; then
            log_success "Compilation error detected correctly (no run stage)"
        else
            log_error "Expected COMPILATION_ERROR, got: $compile_status, run: $run"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

if [ -n "$C_VERSION" ]; then
    test_start "Compilation Error (C - Missing Header)"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "int main() {\n    printf(\"Hello\\n\");\n    return 0;\n}"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        compile_status=$(echo "$result" | jq -r '.result.compile.status')
        
        if [ "$compile_status" = "COMPILATION_ERROR" ]; then
            log_success "C compilation error detected correctly"
        else
            log_error "Expected COMPILATION_ERROR, got: $compile_status"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 8. SECURITY & RESOURCE LIMITS (SIGKILL scenarios)
# ----------------------------------------------------------------------------

if [ -n "$C_VERSION" ]; then
    test_start "Fork Bomb Protection"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "#include <unistd.h>\nint main() {\n    while(1) {\n        fork();\n    }\n    return 0;\n}"
                }
            ],
            "run_timeout": 3000
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        run_status=$(echo "$result" | jq -r '.result.run.status')
        signal=$(echo "$result" | jq -r '.result.run.signal')
        
        # Fork bomb should be killed by resource limits
        if [ "$run_status" = "RUNTIME_ERROR" ] || [ "$run_status" = "TIME_LIMIT_EXCEEDED" ]; then
            log_success "Fork bomb was contained (status: $run_status)"
        else
            log_error "Fork bomb test failed. Status: $run_status, Signal: $signal"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# 9. VALIDATION TESTS
# ----------------------------------------------------------------------------

sleep 2  # Breathe before validation tests

test_start "Validation: No Files"
response=$(curl -s -w "\n%{http_code}" -X POST "$BASE_URL/jobs" \
    -H "Content-Type: application/json" \
    -d '{
        "language": "python",
        "version": "3.12.0",
        "files": []
    }')
http_code=$(echo "$response" | tail -n1)
body=$(echo "$response" | head -n-1)

if [ "$http_code" = "400" ] && echo "$body" | grep -q "at least one file"; then
    log_success "Validation correctly rejected empty files array"
else
    log_error "Expected 400 with 'at least one file' error, got: $http_code"
fi

sleep 1

test_start "Validation: Unsupported Runtime"
response=$(curl -s -w "\n%{http_code}" -X POST "$BASE_URL/jobs" \
    -H "Content-Type: application/json" \
    -d '{
        "language": "nonexistent-lang",
        "version": "1.0.0",
        "files": [{"name": "test", "content": "test"}]
    }')
http_code=$(echo "$response" | tail -n1)

if [ "$http_code" = "400" ]; then
    log_success "Validation correctly rejected unsupported runtime"
else
    log_error "Expected 400 for unsupported runtime, got: $http_code"
fi

sleep 1

test_start "Get Non-Existent Job"
response=$(curl -s -w "\n%{http_code}" "$BASE_URL/jobs/00000000-0000-0000-0000-000000000000")
http_code=$(echo "$response" | tail -n1)

if [ "$http_code" = "404" ]; then
    log_success "Correctly returned 404 for non-existent job"
else
    log_error "Expected 404 for non-existent job, got: $http_code"
fi

# ----------------------------------------------------------------------------
# 10. COMPLEX MULTI-FILE TESTS
# ----------------------------------------------------------------------------

if [ -n "$C_VERSION" ]; then
    test_start "Multi-File C Program"
    response=$(submit_job '{
            "language": "c",
            "version": "'"$C_VERSION"'",
            "files": [
                {
                    "name": "main.c",
                    "content": "#include <stdio.h>\n#include \"helper.h\"\nint main() {\n    int result = add(5, 3);\n    printf(\"%d\", result);\n    return 0;\n}"
                },
                {
                    "name": "helper.h",
                    "content": "int add(int a, int b);"
                },
                {
                    "name": "helper.c",
                    "content": "int add(int a, int b) {\n    return a + b;\n}"
                }
            ]
        }')
    
    job_id=$(echo "$response" | jq -r '.job_id')
    if [ -n "$job_id" ] && [ "$job_id" != "null" ]; then
        log_info "Job submitted: $job_id"
        result=$(wait_for_job "$job_id" $TIMEOUT)
        compile_status=$(echo "$result" | jq -r '.result.compile.status')
        run_status=$(echo "$result" | jq -r '.result.run.status')
        stdout=$(echo "$result" | jq -r '.result.run.stdout')
        
        if [ "$compile_status" = "SUCCESS" ] && [ "$run_status" = "SUCCESS" ] && echo "$stdout" | grep -q "8"; then
            log_success "Multi-file C program compiled and executed correctly"
        else
            log_error "Multi-file test failed. Compile: $compile_status, Run: $run_status, Output: $stdout"
        fi
    else
        log_error "Failed to submit job: $response"
    fi
fi

# ----------------------------------------------------------------------------
# SUMMARY
# ----------------------------------------------------------------------------

echo ""
echo "======================================================================"
echo "                          TEST SUMMARY"
echo "======================================================================"
echo "Total Tests: $TEST_COUNT"
echo -e "${GREEN}Passed: $PASS_COUNT${NC}"
echo -e "${RED}Failed: $FAIL_COUNT${NC}"
echo "======================================================================"

if [ $FAIL_COUNT -eq 0 ]; then
    echo -e "${GREEN}✓ All tests passed!${NC}"
    exit 0
else
    echo -e "${RED}✗ Some tests failed.${NC}"
    exit 1
fi
