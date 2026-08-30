//go:build js && wasm

// Copyright 2026 Luvus contributors
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"sync"
	"sync/atomic"
	"syscall/js"
	"time"

	"github.com/tailscale/tailcat"
	"tailscale.com/types/logger"
)

const (
	connectTimeout = 60 * time.Second
	dialTimeout    = 30 * time.Second
	maxWriteBytes  = 1 << 20
)

func main() {
	js.Global().Set("luvusTailcatConnect", js.FuncOf(connect))
	if ready := js.Global().Get("onLuvusTailcatReady"); ready.Type() == js.TypeFunction {
		ready.Invoke()
	}
	select {}
}

func connect(_ js.Value, args []js.Value) any {
	if len(args) != 1 || args[0].Type() != js.TypeObject {
		return reject(errors.New("connection options are required"))
	}
	address := optionString(args[0], "address")
	if address == "" {
		return reject(errors.New("Tailcat address is required"))
	}
	return promise(func() (any, error) {
		client := &tailcat.Client{
			Server: tailcat.ConnBlob(address),
			Logf:   logger.Discard,
		}
		ctx, cancel := context.WithTimeout(context.Background(), connectTimeout)
		defer cancel()
		if err := pingUntil(ctx, client); err != nil {
			client.Close()
			return nil, fmt.Errorf("Tailcat connection failed: %w", err)
		}
		return makeClient(client), nil
	})
}

func pingUntil(ctx context.Context, client *tailcat.Client) error {
	for {
		attempt, cancel := context.WithTimeout(ctx, 5*time.Second)
		_, err := client.Ping(attempt)
		cancel()
		if err == nil {
			return nil
		}
		if ctx.Err() != nil {
			return errors.New("connection deadline exceeded")
		}
	}
}

func makeClient(client *tailcat.Client) js.Value {
	var closed atomic.Bool
	var functions []js.Func
	release := sync.OnceFunc(func() { releaseFunctions(functions) })
	dial := js.FuncOf(func(_ js.Value, args []js.Value) any {
		if closed.Load() {
			return reject(errors.New("Tailcat client is closed"))
		}
		if len(args) != 1 || args[0].Type() != js.TypeNumber {
			return reject(errors.New("dial requires one port"))
		}
		port := args[0].Int()
		if port < 1 || port > 65535 {
			return reject(errors.New("port must be between 1 and 65535"))
		}
		return promise(func() (any, error) {
			ctx, cancel := context.WithTimeout(context.Background(), dialTimeout)
			defer cancel()
			connection, err := client.DialTCPPort(ctx, uint16(port))
			if err != nil {
				return nil, errors.New("Tailcat stream could not be opened")
			}
			return makeConnection(connection), nil
		})
	})
	closeClient := js.FuncOf(func(_ js.Value, _ []js.Value) any {
		if closed.CompareAndSwap(false, true) {
			client.Close()
		}
		release()
		return nil
	})
	functions = []js.Func{dial, closeClient}
	return js.ValueOf(map[string]any{
		"dial":  dial,
		"close": closeClient,
	})
}

func makeConnection(connection net.Conn) js.Value {
	var closed atomic.Bool
	var reading atomic.Bool
	var writeMu sync.Mutex
	buffer := make([]byte, 64<<10)
	var functions []js.Func
	release := sync.OnceFunc(func() { releaseFunctions(functions) })
	read := js.FuncOf(func(_ js.Value, _ []js.Value) any {
		if !reading.CompareAndSwap(false, true) {
			return reject(errors.New("concurrent reads are not allowed"))
		}
		return promise(func() (any, error) {
			defer reading.Store(false)
			n, err := connection.Read(buffer)
			if n > 0 {
				data := js.Global().Get("Uint8Array").New(n)
				js.CopyBytesToJS(data, buffer[:n])
				return data, nil
			}
			if err == nil || errors.Is(err, io.EOF) {
				return js.Null(), nil
			}
			return nil, errors.New("Tailcat stream read failed")
		})
	})
	write := js.FuncOf(func(_ js.Value, args []js.Value) any {
		if len(args) != 1 || args[0].InstanceOf(js.Global().Get("Uint8Array")) == false {
			return reject(errors.New("write requires a Uint8Array"))
		}
		length := args[0].Get("length").Int()
		if length < 0 || length > maxWriteBytes {
			return reject(errors.New("write exceeds the one MiB limit"))
		}
		data := make([]byte, length)
		js.CopyBytesToGo(data, args[0])
		return promise(func() (any, error) {
			writeMu.Lock()
			defer writeMu.Unlock()
			for len(data) > 0 {
				n, err := connection.Write(data)
				if err != nil {
					return nil, errors.New("Tailcat stream write failed")
				}
				data = data[n:]
			}
			return js.Undefined(), nil
		})
	})
	closeWrite := js.FuncOf(func(_ js.Value, _ []js.Value) any {
		return promise(func() (any, error) {
			closer, ok := connection.(interface{ CloseWrite() error })
			if !ok {
				return nil, errors.New("Tailcat stream cannot be half-closed")
			}
			if err := closer.CloseWrite(); err != nil {
				return nil, errors.New("Tailcat stream half-close failed")
			}
			return js.Undefined(), nil
		})
	})
	closeConnection := js.FuncOf(func(_ js.Value, _ []js.Value) any {
		if closed.CompareAndSwap(false, true) {
			connection.Close()
		}
		release()
		return nil
	})
	functions = []js.Func{read, write, closeWrite, closeConnection}
	return js.ValueOf(map[string]any{
		"read":       read,
		"write":      write,
		"closeWrite": closeWrite,
		"close":      closeConnection,
	})
}

func releaseFunctions(functions []js.Func) {
	// Releasing the callback currently executing is unsafe. Yield back to the
	// browser event loop, then release every registration deterministically.
	go func() {
		time.Sleep(time.Millisecond)
		for _, function := range functions {
			function.Release()
		}
	}()
}

func optionString(value js.Value, name string) string {
	property := value.Get(name)
	if property.Type() == js.TypeString {
		return property.String()
	}
	return ""
}

func promise(work func() (any, error)) js.Value {
	var handler js.Func
	handler = js.FuncOf(func(_ js.Value, args []js.Value) any {
		resolve, reject := args[0], args[1]
		go func() {
			defer handler.Release()
			if result, err := work(); err == nil {
				resolve.Invoke(result)
			} else {
				reject.Invoke(js.Global().Get("Error").New(err.Error()))
			}
		}()
		return nil
	})
	return js.Global().Get("Promise").New(handler)
}

func reject(err error) js.Value {
	return js.Global().Get("Promise").Call("reject", js.Global().Get("Error").New(err.Error()))
}
