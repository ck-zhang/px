# px pythonstress set

Global goal for all pythonstress repos:

- Treat each target repo purely as a px-managed project (px owns `pyproject.toml`/`px.lock`/`.px`), not as a px tool integration.
- The success criteria per repo are: “can we drive its normal dev / test workflow smoothly via px?” — i.e. `px sync`, `px status`, `px run`/`px test` (and any repo-specific scripts) behave as expected.
- We do **not** require the repo’s own CLI to be available as a px tool; pythonstress is about exercising px on real-world projects, not extending px’s command surface.

Repos to exercise with px under `/home/toxictoast/test/pythonstress`, grouped and ordered:

## Core packaging / build tools
- https://github.com/pypa/pip.git
- https://github.com/pypa/build.git
- https://github.com/pypa/hatch.git
- https://github.com/python-poetry/poetry.git
- https://github.com/pypa/flit.git
- https://github.com/pdm-project/pdm.git
- https://github.com/astral-sh/uv.git

## Small pure-Python libs
- https://github.com/psf/requests.git
- https://github.com/urllib3/urllib3.git
- https://github.com/kjd/idna.git
- https://github.com/Ousret/charset_normalizer.git
- https://github.com/pallets/click.git
- https://github.com/python-attrs/attrs.git

## Web frameworks
- https://github.com/django/django.git
- https://github.com/pallets/flask.git
- https://github.com/tiangolo/fastapi.git
- https://github.com/encode/starlette.git

## Plugin-heavy ecosystems
- https://github.com/pytest-dev/pytest.git
- https://github.com/tox-dev/tox.git
- https://github.com/sphinx-doc/sphinx.git
- https://github.com/pypa/setuptools.git

## Scientific stack
- https://github.com/numpy/numpy.git
- https://github.com/pandas-dev/pandas.git
- https://github.com/scipy/scipy.git
- https://github.com/scikit-learn/scikit-learn.git
- https://github.com/matplotlib/matplotlib.git

## Hard-to-build extensions
- https://github.com/apache/arrow.git
- https://github.com/ijl/orjson.git
- https://github.com/MagicStack/uvloop.git
- https://github.com/psycopg/psycopg2.git
- https://github.com/lxml/lxml.git
- https://github.com/h5py/h5py.git
- https://github.com/pyca/cryptography.git

## Large dependency-graph applications
- https://github.com/jupyterlab/jupyterlab.git
- https://github.com/spyder-ide/spyder.git
- https://github.com/apache/airflow.git
- https://github.com/ray-project/ray.git
- https://github.com/huggingface/transformers.git

## CLI dev tools
- https://github.com/psf/black.git
- https://github.com/astral-sh/ruff.git
- https://github.com/python/mypy.git
- https://github.com/PyCQA/isort.git
- https://github.com/pre-commit/pre-commit.git

## Namespace ecosystem examples
- https://github.com/googleapis/python-storage.git
- https://github.com/googleapis/python-bigquery.git
- https://github.com/Azure/azure-sdk-for-python.git
- https://github.com/boto/boto3.git
- https://github.com/boto/botocore.git

## Workflow notes: poetry (python-poetry/poetry)
- Location: `/home/toxictoast/test/pythonstress/poetry` (requires Python >=3.9,<4.0).
- For pythonstress, we treat Poetry as a px project and configure `[tool.px.dependencies].include-groups = ["dev", "test", "typing"]` in its `pyproject.toml` so pytest and typing/test deps are part of the px lock/env.
- With that config in place, px status is clean after the marker-aware drift fix (tomli/importlib-metadata/xattr no longer flagged); `px sync` reports `px.lock` already up to date.
- `px test` then runs Poetry’s tests under px; use `px test -- -k version` (or similar selectors) to keep the run lightweight in the stress suite.
- We do not require the Poetry CLI itself to be runnable inside px for this scenario; the goal is that `px sync`, `px status`, and `px test` all behave correctly on the repo.
