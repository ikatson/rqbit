#!/usr/bin/env bash

# end on sigint
STOP=""
trap "STOP=1" INT


NUM_RUNS=${1:-100}
TOTAL=$NUM_RUNS
FAILURES=0
export NO_COLOR=1
rm /tmp/librqbit_e2e_failures.log
while [ $NUM_RUNS -gt 0 ]; do
    echo Running test $((TOTAL - NUM_RUNS + 1)) of $TOTAL
    NUM_RUNS=$((NUM_RUNS - 1))
    cargo test --package librqbit  tests::e2e::test_e2e_download > /tmp/librqbit_e2e_run.log
    res=$?
    if [ $res -ne 0 ]; then
        FAILURES=$((FAILURES + 1)) 
        echo "FAILURE with code $res" >> /tmp/librqbit_e2e_failures.log
        tail -50 /tmp/librqbit_e2e_run.log >> /tmp/librqbit_e2e_failures.log
        echo "----------------------------------------------" >> /tmp/librqbit_e2e_failures.log

    fi
    if [ "$STOP" = "1" ]; then
        break
    fi
done
rm /tmp/librqbit_e2e_run.log
echo "Total runs : $TOTAL, failures : $FAILURES"
echo "Failures log /tmp/librqbit_e2e_failures.log" 