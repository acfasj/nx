import type { BuilderContext } from '@angular-devkit/architect';
import type { DevServerBuilderOptions } from '@angular-devkit/build-angular';
import {
  joinPathFragments,
  parseTargetString,
  readCachedProjectGraph,
  type TargetConfiguration,
} from '@nx/devkit';
import { getRootTsConfigPath } from '@nx/js';
import type { DependentBuildableProjectNode } from '@nx/js/src/utils/buildable-libs-utils';
import { WebpackNxBuildCoordinationPlugin } from '@nx/webpack/src/plugins/webpack-nx-build-coordination-plugin';
import { existsSync } from 'fs';
import { isNpmProject } from 'nx/src/project-graph/operators';
import { readCachedProjectConfiguration } from 'nx/src/project-graph/project-graph';
import { from } from 'rxjs';
import { switchMap } from 'rxjs/operators';
import { gte } from 'semver';
import { getInstalledAngularVersionInfo } from '../../executors/utilities/angular-version-utils';
import { createTmpTsConfigForBuildableLibs } from '../utilities/buildable-libs';
import {
  mergeCustomWebpackConfig,
  resolveIndexHtmlTransformer,
} from '../utilities/webpack';
import { normalizeOptions } from './lib';
import type {
  NormalizedSchema,
  Schema,
  SchemaWithBrowserTarget,
} from './schema';

type BuildTargetOptions = {
  tsConfig: string;
  buildLibsFromSource?: boolean;
  customWebpackConfig?: { path?: string };
  indexFileTransformer?: string;
};

export function executeDevServerBuilder(
  rawOptions: Schema,
  context: import('@angular-devkit/architect').BuilderContext
) {
  process.env.NX_TSCONFIG_PATH = getRootTsConfigPath();

  const options = normalizeOptions(rawOptions);

  const parsedBuildTarget = parseTargetString(options.buildTarget, {
    cwd: context.currentDirectory,
    projectGraph: readCachedProjectGraph(),
    projectName: context.target.project,
    root: context.workspaceRoot,
    isVerbose: false,
  });
  const browserTargetProjectConfiguration = readCachedProjectConfiguration(
    parsedBuildTarget.project
  );

  const buildTarget =
    browserTargetProjectConfiguration.targets[parsedBuildTarget.target];

  const buildTargetOptions: BuildTargetOptions = {
    ...buildTarget.options,
    ...(parsedBuildTarget.configuration
      ? buildTarget.configurations[parsedBuildTarget.configuration]
      : buildTarget.defaultConfiguration
      ? buildTarget.configurations[buildTarget.defaultConfiguration]
      : {}),
  };

  const buildLibsFromSource =
    options.buildLibsFromSource ??
    buildTargetOptions.buildLibsFromSource ??
    true;

  let pathToWebpackConfig: string;
  if (buildTargetOptions.customWebpackConfig?.path) {
    pathToWebpackConfig = joinPathFragments(
      context.workspaceRoot,
      buildTargetOptions.customWebpackConfig.path
    );

    if (pathToWebpackConfig && !existsSync(pathToWebpackConfig)) {
      throw new Error(
        `Custom Webpack Config File Not Found!\nTo use a custom webpack config, please ensure the path to the custom webpack file is correct: \n${pathToWebpackConfig}`
      );
    }
  }

  let pathToIndexFileTransformer: string;
  if (buildTargetOptions.indexFileTransformer) {
    pathToIndexFileTransformer = joinPathFragments(
      context.workspaceRoot,
      buildTargetOptions.indexFileTransformer
    );

    if (pathToIndexFileTransformer && !existsSync(pathToIndexFileTransformer)) {
      throw new Error(
        `File containing Index File Transformer function Not Found!\n Please ensure the path to the file containing the function is correct: \n${pathToIndexFileTransformer}`
      );
    }
  }

  let dependencies: DependentBuildableProjectNode[];
  if (!buildLibsFromSource) {
    const { tsConfigPath, dependencies: foundDependencies } =
      createTmpTsConfigForBuildableLibs(buildTargetOptions.tsConfig, context, {
        target: parsedBuildTarget.target,
      });
    dependencies = foundDependencies;

    // We can't just pass the tsconfig path in memory to the angular builder
    // function because we can't pass the build target options to it, the build
    // targets options will be retrieved by the builder from the project
    // configuration. Therefore, we patch the method in the context to retrieve
    // the target options to overwrite the tsconfig path to use the generated
    // one with the updated path mappings.
    const originalGetTargetOptions = context.getTargetOptions;
    context.getTargetOptions = async (target) => {
      const options = await originalGetTargetOptions(target);
      options.tsConfig = tsConfigPath;
      return options;
    };

    // The buildTargetConfiguration also needs to use the generated tsconfig path
    // otherwise the build will fail if customWebpack function/file is referencing
    // local libs. This synchronize the behavior with webpack-browser and
    // webpack-server implementation.
    buildTargetOptions.tsConfig = tsConfigPath;
  }

  const delegateBuilderOptions = getDelegateBuilderOptions(
    options,
    buildTarget,
    context
  );
  const isUsingWebpackBuilder = ![
    '@angular-devkit/build-angular:application',
    '@angular-devkit/build-angular:browser-esbuild',
    '@nx/angular:browser-esbuild',
  ].includes(buildTarget.executor);

  return from(import('@angular-devkit/build-angular')).pipe(
    switchMap(({ executeDevServerBuilder }) =>
      executeDevServerBuilder(delegateBuilderOptions, context, {
        webpackConfiguration: isUsingWebpackBuilder
          ? async (baseWebpackConfig) => {
              if (!buildLibsFromSource) {
                const workspaceDependencies = dependencies
                  .filter((dep) => !isNpmProject(dep.node))
                  .map((dep) => dep.node.name);
                // default for `nx run-many` is --all projects
                // by passing an empty string for --projects, run-many will default to
                // run the target for all projects.
                // This will occur when workspaceDependencies = []
                if (workspaceDependencies.length > 0) {
                  baseWebpackConfig.plugins.push(
                    // @ts-expect-error - difference between angular and webpack plugin definitions bc of webpack versions
                    new WebpackNxBuildCoordinationPlugin(
                      `nx run-many --target=${
                        parsedBuildTarget.target
                      } --projects=${workspaceDependencies.join(',')}`
                    )
                  );
                }
              }

              if (!pathToWebpackConfig) {
                return baseWebpackConfig;
              }

              return mergeCustomWebpackConfig(
                baseWebpackConfig,
                pathToWebpackConfig,
                buildTargetOptions,
                context.target
              );
            }
          : undefined,

        ...(pathToIndexFileTransformer
          ? {
              indexHtml: resolveIndexHtmlTransformer(
                pathToIndexFileTransformer,
                buildTargetOptions.tsConfig,
                context.target
              ),
            }
          : {}),
      })
    )
  );
}

export default require('@angular-devkit/architect').createBuilder(
  executeDevServerBuilder
) as any;

function getDelegateBuilderOptions(
  options: NormalizedSchema,
  buildTarget: TargetConfiguration,
  context: BuilderContext
) {
  const delegatedBuilderOptions: DevServerBuilderOptions = { ...options };
  const { major: angularMajorVersion, version: angularVersion } =
    getInstalledAngularVersionInfo();

  // this option was introduced in angular 16.1.0
  // https://github.com/angular/angular-cli/commit/3ede1a2cac5005f4dfbd2a62ef528a34c3793b78
  if (
    gte(angularVersion, '16.1.0') &&
    buildTarget.executor === '@nx/angular:browser-esbuild'
  ) {
    delegatedBuilderOptions.forceEsbuild = true;

    const originalLoggerWarn = context.logger.warn.bind(context.logger);
    context.logger.warn = (...args) => {
      // we silence the warning about forcing esbuild from third-party builders
      if (
        args[0].includes(
          'Warning: Forcing the use of the esbuild-based build system with third-party builders may cause unexpected behavior and/or build failures.'
        )
      ) {
        return;
      }

      originalLoggerWarn(...args);
    };
  }

  if (angularMajorVersion <= 17) {
    (
      delegatedBuilderOptions as unknown as SchemaWithBrowserTarget
    ).browserTarget = delegatedBuilderOptions.buildTarget;
    delete delegatedBuilderOptions.buildTarget;
  }

  return delegatedBuilderOptions;
}
