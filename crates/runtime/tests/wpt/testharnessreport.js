// Custom testharnessreport.js for V8 embedding.
// Collects test results into __wpt_results global for Rust to read.

var __wpt_results = [];
var __wpt_done = false;

add_completion_callback(function(tests, harness_status) {
    __wpt_test_count = tests.length;
    for (var i = 0; i < tests.length; i++) {
        var t = tests[i];
        __wpt_results.push({
            name: t.name,
            status: t.status,       // 0=PASS, 1=FAIL, 2=TIMEOUT, 3=NOTRUN
            message: t.message || null,
        });
    }
    __wpt_done = true;
});
var __wpt_test_count = 0;
