package ctl

import (
	"errors"
	"fmt"
	"io"
	"time"
)

// Actions implement `patina ctl`. Every event received is written to out as
// its raw NDJSON line, so scripts can consume the output directly.

func printer(out io.Writer) func(*Event) {
	return func(e *Event) {
		if out != nil {
			fmt.Fprintln(out, e.Raw)
		}
	}
}

// GuestError is an {"ev":"error"} reply.
type GuestError struct{ Msg string }

func (e *GuestError) Error() string { return "guest: " + e.Msg }

// DoStop sends stop and waits for the exit event, returning its code.
func DoStop(c *Client, grace, timeout time.Duration, out io.Writer) (int, error) {
	if err := c.Send(Stop(grace)); err != nil {
		return 0, err
	}
	var deadline time.Time
	if timeout > 0 {
		deadline = time.Now().Add(timeout)
	}
	e, err := c.WaitFor(deadline, printer(out), func(e *Event) bool { return e.Ev == "exit" || e.Ev == "error" })
	if err != nil {
		if errors.Is(err, io.EOF) {
			return 0, errors.New("control channel closed before the guest reported an exit")
		}
		return 0, err
	}
	if e.Ev == "error" {
		return 0, &GuestError{e.Msg}
	}
	if e.Code == nil {
		return 0, fmt.Errorf("exit event without a code: %s", e.Raw)
	}
	return *e.Code, nil
}

// DoPing sends ping with id and waits for the matching pong.
func DoPing(c *Client, id uint64, timeout time.Duration, out io.Writer) (*Event, error) {
	if err := c.Send(Ping(id)); err != nil {
		return nil, err
	}
	e, err := c.WaitFor(time.Now().Add(timeout), printer(out), func(e *Event) bool {
		return (e.Ev == "pong" || e.Ev == "error") && (e.ID == nil || *e.ID == id)
	})
	if err != nil {
		return nil, err
	}
	if e.Ev == "error" {
		return nil, &GuestError{e.Msg}
	}
	return e, nil
}

// DoOneWay sends an op that has no acknowledgement (signal, dump, time) and
// prints events for linger, failing if the guest replies with an error.
func DoOneWay(c *Client, op Op, linger time.Duration, out io.Writer) error {
	if err := c.Send(op); err != nil {
		return err
	}
	if linger <= 0 {
		return nil
	}
	e, err := c.WaitFor(time.Now().Add(linger), printer(out), func(e *Event) bool { return e.Ev == "error" || e.Ev == "exit" })
	switch {
	case errors.Is(err, ErrTimeout), errors.Is(err, io.EOF):
		return nil
	case err != nil:
		return err
	case e.Ev == "error":
		return &GuestError{e.Msg}
	}
	return nil
}
