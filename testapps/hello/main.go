// hello prints a greeting and its arguments, then exits with the code given
// as its first argument (default 0).
//
//	hello        -> "PASS hello", exit 0
//	hello 3 a b  -> "PASS hello", exit 3
//
// It also prints its environment as `env: ["K=V" ...]` (patina run --env).
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	fmt.Println("hello from patina")
	fmt.Printf("argv: %q\n", os.Args)
	fmt.Printf("env: %q\n", os.Environ())
	code := 0
	if len(os.Args) > 1 {
		n, err := strconv.Atoi(os.Args[1])
		if err != nil || n < 0 || n > 255 {
			fmt.Printf("FAIL hello: bad exit code argument %q\n", os.Args[1])
			os.Exit(1)
		}
		code = n
	}
	fmt.Printf("PASS hello (exit %d)\n", code)
	os.Exit(code)
}
