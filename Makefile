.PHONY: app dmg install run test clean

app:
	./setup.sh

dmg:
	./build-dmg.sh

install:
	./setup.sh --install-app

run:
	./run.sh

test:
	cargo test

clean:
	cargo clean
	rm -rf dist
